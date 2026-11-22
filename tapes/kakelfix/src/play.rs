//! The Play page's tape side: the projects and takes on the page's disk (under its root folder), a take's files read
//! through the path calls (`tape_rt::source::FileSource`, any volume) and decoded (`audio_decode`: FLAC up to 8 channels
//! a file, WAV every input of the take), the inputs listened to mixed to the output pair (`kakelfix::play::Playing::mix`)
//! and written to the device's playback side, the whole take read through a second time in the background for its overview.
//!
//! Playback runs on the drawing thread, a bounded amount per pass: it is kept about `AHEAD_MS` ahead of the device.
//! The overview is read by a thread of its own where the kernel runs tape threads (the server kernel): `Scanner`, which
//! reads `SCAN_FRAMES` of each file at a time and rests (`tape_rt::idle`) in between, so the drawing thread keeps the
//! CPU even when both are pinned to the same one. Without threads the drawing thread scans for at most `INLINE_US` a
//! pass. The overview is redrawn at most every `REDRAW_US` while it fills.
//!
//! The stream is started with playback when the device has a playback side (`analyze::Capture`); when it is not (a
//! loopback whose playback end another tape holds), the page says so and only shows takes.

use crate::view::{centered, text};
use audio_decode::{Decode, flac, wav};
use core::fmt::Write;
use fosfor_abi::tape::{AudioInfo, TapeApi, VFS_DIR};
use gfx::Canvas;
use gfx::theme::theme;
use kakelfix::analyzer::Envelope;
use kakelfix::naming::{self, MAX_PATH, ProjectName};
use kakelfix::peaks;
use kakelfix::play::{MAX_FILES, MAX_LIST, MAX_ROWS, Overview, TakeFile, TakeName, default_output, take_file};
use kakelfix::{App, Focus, Layout, PlayButton, PlayRequest, PlayState, Playing, Rect, Text};
use mixer_model::Group;
use tape_rt::alloc_zeroed;
use tape_rt::channel::Handover;
use tape_rt::source::FileSource;

/// Playback is kept this far ahead of the device: enough to ride out a slow pass, little enough that a pause, a seek or
/// a stop is heard at once.
const AHEAD_MS: u64 = 150;
/// Frames the overview reads of each file before it rests (thread) or looks at the time (inline).
const SCAN_FRAMES: usize = 8_192;
/// The most the drawing thread scans a pass when there is no scanner thread.
const INLINE_US: u64 = 3_000;
/// How often the page is redrawn for the overview's progress alone.
const REDRAW_US: u64 = 250_000;
/// How long `hold` waits for the scanner to let go of its readers (it does within one read).
const HOLD_US: u64 = 500_000;
const SCANNER_STACK: usize = 64 * 1024;
/// Frames mixed and written per step.
const STEP: usize = 512;
/// Seconds the live envelope spans.
const LIVE_S: f32 = 4.0;

type Flac = flac::Decoder<FileSource, 8>;
/// A WAV take is one file of every armed input.
type Wav = wav::Decoder<FileSource, { MAX_ROWS }>;

/// Where a reader's frames come from: a decoder's block.
enum Kind {
    Flac(&'static mut Flac),
    Wav(&'static mut Wav),
}

/// One file read from start to end: its frames come out a block at a time (`frames`), `at` is the take's frame of the
/// block's first.
struct Reader {
    kind: Kind,
    file: TakeFile,
    /// The block: frames `start..end` not used yet; `at` the take frame of index 0.
    start: usize,
    end: usize,
    at: u64,
    scale: f32,
}

impl Reader {
    /// Sample `channel` of the block's frame `index` as -1..1.
    fn sample(&self, channel: usize, index: usize) -> f32 {
        match &self.kind {
            Kind::Flac(decoder) => decoder.samples[channel][index] as f32 * self.scale,
            Kind::Wav(decoder) => decoder.sample(channel, index),
        }
    }

    /// Frames ready: refills the block when it is used up. 0 at the end of the file or on a read error.
    fn ready(&mut self) -> usize {
        if self.start < self.end {
            return self.end - self.start;
        }
        self.at += self.end as u64;
        (self.start, self.end) = (0, 0);
        let block = match &mut self.kind {
            Kind::Flac(decoder) => decoder.next_block(),
            Kind::Wav(decoder) => decoder.next_block(),
        };
        if let Ok(Some(count)) = block {
            self.end = count;
        }
        self.end
    }

    /// Takes `count` frames of the block.
    fn take(&mut self, count: usize) {
        self.start = (self.start + count).min(self.end);
    }

    /// The take frame the next `sample` index `start` is.
    fn position(&self) -> u64 {
        self.at + self.start as u64
    }

    /// Goes to take frame `frame`: a FLAC file to the block holding it, a WAV file straight there.
    fn seek(&mut self, frame: u64) {
        match &mut self.kind {
            Kind::Flac(decoder) => {
                let first = decoder.seek(frame).unwrap_or(0);
                let count = decoder.next_block().ok().flatten().unwrap_or(0);
                (self.at, self.end) = (first, count);
                self.start = (frame.saturating_sub(first) as usize).min(count);
            }
            // Exact: the next block starts at the frame.
            Kind::Wav(decoder) => {
                let first = decoder.seek(frame).unwrap_or(0);
                (self.at, self.start, self.end) = (first, 0, 0);
            }
        }
    }
}

/// The page's tape side (one, from the heap): readers for playing and for the overview, the overview and the live envelope.
pub struct Player {
    play: [Option<Reader>; MAX_FILES],
    /// The scanner's thread, when the kernel runs tape threads (started with the first take).
    thread: Option<tape_rt::Thread>,
    tried_thread: bool,
    /// The overview was read on the scanner thread (kept after `quit` joins it, for the summary).
    threaded: bool,
    /// The scanner thread holds off (`hold` succeeded, `release` not yet called): its static is the drawing thread's.
    held: bool,
    /// When the page was last redrawn for the overview's progress.
    shown_at: u64,
    /// Frames played since the tape started (for the line it prints at the end).
    played: u64,
    /// Files of the open take whose overview comes from their peak file.
    from_peaks: usize,
    /// Decoders made so far (they are kept for the next take; the heap gives nothing back). A WAV take is one file.
    decoders: [Option<&'static mut Flac>; 2 * MAX_FILES],
    wavs: [Option<&'static mut Wav>; 2],
    overview: &'static Overview,
    live: &'static mut Envelope,
    live_columns: usize,
    /// Each file's channel to row (`rows[file][channel]`).
    rows: [[u8; 8]; MAX_FILES],
    /// The device's playback frame: channels, slot bytes, float; and the playback ring's room when nothing was queued.
    channels: usize,
    slot: usize,
    float: bool,
    room: u32,
    /// Frames written since playing started at `started`, and the take frame playing started from.
    written: u64,
    started: u64,
    problem: Option<&'static str>,
}

impl Player {
    pub fn new(api: &TapeApi) -> Option<&'static mut Player> {
        let player: &'static mut Player = alloc_zeroed(api)?;
        // Everything in it is numbers or `None` (all zero bytes) except the two buffers, which are numbers too.
        let overview: &'static mut Overview = alloc_zeroed(api)?;
        // Zero bytes are no overview at all (no rows); `configure` gives it its shape when a take opens.
        player.overview = overview;
        player.live = alloc_zeroed(api)?;
        Some(player)
    }

    pub fn overview(&self) -> &Overview {
        self.overview
    }

    pub fn live(&self) -> &Envelope {
        self.live
    }

    pub fn problem(&self) -> Option<&'static str> {
        self.problem
    }

    /// The device's playback side as the stream runs: `Some((channels, slot bytes, float))` with playback, None
    /// without (a capture-only start). Picks the default output pair the first time (`default_output`).
    pub fn set_output(&mut self, app: &mut App, playback: Option<(usize, usize, bool)>) {
        let (channels, slot, float) = playback.unwrap_or((0, 0, false));
        (self.channels, self.slot, self.float) = (channels, slot, float);
        // A new stream: its ring's size is learnt again from the room it shows.
        self.room = 0;
        let p = &mut app.playing;
        p.available = channels >= 2 && slot > 0;
        p.outputs = channels;
        if p.output.is_some_and(|o| o + 1 >= channels) {
            p.output = None;
        }
        if p.available && p.output.is_none() {
            let names = channels.min(mixer_model::MAX_CHANNELS);
            let output = default_output(names, |index| app.name(Group::Output, index));
            app.playing.output = Some(output);
        }
        if !app.playing.available && app.playing.state == PlayState::Playing {
            app.playing.state = PlayState::Paused;
        }
    }

    /// Serves the page's request and plays and reads on; true when the page has something new to show.
    pub fn update(&mut self, api: &TapeApi, app: &mut App, layout: &Layout) -> bool {
        let mut fresh = false;
        if let Some(request) = app.playing.request.take() {
            fresh = true;
            match request {
                PlayRequest::List => self.list(app),
                PlayRequest::Open => {
                    self.open(api, app);
                    if app.playing.autoplay && app.playing.open.is_some() {
                        self.start(api, app);
                    }
                    app.playing.autoplay = false;
                }
                PlayRequest::Play if app.playing.open.is_some() => {
                    if app.playing.position >= app.playing.length {
                        self.seek(app, 0);
                    }
                    self.start(api, app);
                }
                PlayRequest::Pause => app.playing.state = PlayState::Paused,
                PlayRequest::Stop => {
                    app.playing.state = PlayState::Stopped;
                    self.seek(app, 0);
                }
                PlayRequest::Seek(frame) => {
                    self.seek(app, frame);
                    if app.playing.state == PlayState::Playing {
                        // `start` reads on from the new position.
                        self.start(api, app);
                    }
                }
                PlayRequest::Play => {}
            }
        }
        let columns = layout.play_live().w.max(1) as usize;
        if columns != self.live_columns && app.playing.rate > 0 {
            self.live_columns = columns;
            self.live.configure(columns, app.playing.rate, LIVE_S);
        }
        if app.playing.state == PlayState::Playing {
            fresh |= self.pump(api, app);
        }
        if app.playing.open.is_some() && self.overview.progress() < 1.0 {
            if self.thread.is_none() {
                // No thread: a few milliseconds of it a pass.
                let began = tape_rt::micros();
                // SAFETY: without a thread the scanner is the drawing thread's alone.
                if let Some(scanner) = unsafe { SCANNER.state() }.as_mut() {
                    while tape_rt::micros().wrapping_sub(began) < INLINE_US && scanner.step() {}
                }
            }
            let now = tape_rt::micros();
            if now.wrapping_sub(self.shown_at) >= REDRAW_US || self.overview.progress() >= 1.0 {
                self.shown_at = now;
                fresh = true;
            }
        }
        fresh
    }

    /// Asks the scanner thread to let go of its readers and waits until it has (true), so the drawing thread may change
    /// them. Without a thread there is nothing to wait for.
    fn hold(&mut self) -> bool {
        if self.thread.is_none() {
            return true;
        }
        self.held = SCANNER.hold(HOLD_US);
        self.held
    }

    /// Lets the scanner thread go on with the readers it has now.
    fn release(&mut self) {
        self.held = false;
        if self.thread.is_some() {
            SCANNER.release();
        }
    }

    /// The line printed when the tape ends, if a take was opened: what was played and how much of the overview read.
    pub fn summary(&self, app: &App) -> Option<Text<160>> {
        let (project, take) = app.playing.open?;
        let mut line = Text::new();
        let rate = u64::from(app.playing.rate.max(1));
        let _ = write!(
            line,
            "kakelfix: played {} / {}: {}.{} s, overview read {}% ({})",
            project.as_str(),
            take.as_str(),
            self.played / rate,
            self.played % rate * 10 / rate,
            (self.overview.progress() * 100.0) as u32,
            match (self.from_peaks, app.playing.file_count) {
                (0, _) => if self.threaded { "from the audio, on its own thread" } else { "from the audio, inline" },
                (n, files) if n == files => "from its peak file",
                _ => "partly from peak files",
            }
        );
        Some(line)
    }

    /// At the end of the tape: stops the scanner thread.
    pub fn quit(&mut self) {
        if let Some(thread) = self.thread.take() {
            SCANNER.quit();
            thread.join();
        }
    }

    /// The projects under the root folder of the page's disk, sorted by name, and the takes of the picked one, the
    /// newest first.
    fn list(&mut self, app: &mut App) {
        let base = base_path(app);
        let p = &mut app.playing;
        let picked = p.projects.get(p.project).copied().filter(|_| p.project < p.project_count);
        p.project_count = 0;
        each_entry(base.as_str(), |name, folder| {
            if folder && naming::valid_project_folder(name) && p.project_count < MAX_LIST {
                p.projects[p.project_count] = ProjectName::from(name);
                p.project_count += 1;
            }
        });
        p.projects[..p.project_count].sort_unstable_by(|a, b| {
            a.as_str().bytes().map(|c| c.to_ascii_lowercase()).cmp(b.as_str().bytes().map(|c| c.to_ascii_lowercase()))
        });
        p.project = picked.and_then(|name| p.projects[..p.project_count].iter().position(|&n| n == name)).unwrap_or(p.project.min(p.project_count.saturating_sub(1)));
        p.take_count = 0;
        if p.project < p.project_count {
            let folder = naming::join(&[base.as_str(), p.projects[p.project].as_str()]);
            each_entry(folder.as_str(), |name, is_folder| {
                if is_folder && naming::take_number(name).is_some() && p.take_count < MAX_LIST {
                    p.takes[p.take_count] = TakeName::from(name);
                    p.take_count += 1;
                }
            });
            p.takes[..p.take_count].sort_unstable_by_key(|take| core::cmp::Reverse(naming::take_number(take.as_str()).unwrap_or(0)));
        }
        p.take = p.take.min(p.take_count.saturating_sub(1));
        if p.project_count == 0 {
            app.message.clear();
            let _ = write!(app.message, "no projects in {}", base.as_str());
        }
    }

    /// Opens the take picked in the list: its files, two readers each (play, overview).
    fn open(&mut self, api: &TapeApi, app: &mut App) {
        let base = base_path(app);
        let p = &mut app.playing;
        if p.project >= p.project_count || p.take >= p.take_count {
            return;
        }
        (p.state, p.open, p.position, p.length) = (PlayState::Stopped, None, 0, 0);
        self.play = [const { None }; MAX_FILES];
        self.problem = None;
        if !self.tried_thread {
            self.tried_thread = true;
            if tape_rt::threads_supported() {
                self.thread = tape_rt::spawn_thread(scanner_thread, 0, SCANNER_STACK).ok();
                self.threaded = self.thread.is_some();
            }
        }
        // The scanner's readers use decoders that are about to be reused: it lets go of them first.
        if !self.hold() {
            return self.fail(app, "the overview's reader did not stop (try again)");
        }
        // SAFETY: held (or no thread): the drawing thread has the scanner to itself until `release`.
        unsafe { *SCANNER.state() = None };
        let (project, take) = (p.projects[p.project], p.takes[p.take]);
        let folder = naming::join(&[base.as_str(), project.as_str(), take.as_str()]);
        let mut files = [TakeFile { name: naming::FileName::new(), first: 0, channels: 0, flac: false }; MAX_FILES];
        let mut count = 0;
        each_entry(folder.as_str(), |name, is_folder| {
            if let Some(file) = take_file(name).filter(|_| !is_folder && count < MAX_FILES) {
                files[count] = file;
                count += 1;
            }
        });
        let p = &mut app.playing;
        p.set_files(&files[..count]);
        if count == 0 {
            return self.fail(app, "no audio files in this take");
        }
        let (mut rate, mut length) = (0u32, u64::MAX);
        let mut scans: [Option<ScanSource>; MAX_FILES] = [const { None }; MAX_FILES];
        self.from_peaks = 0;
        for index in 0..p.file_count {
            let file = p.files[index];
            let path = naming::join(&[folder.as_str(), file.name.as_str()]);
            let size = tape_rt::vfs_stat(path.as_str()).map(|stat| stat.size).unwrap_or(0);
            let mut scan = peak_source(folder.as_str(), file).map(|source| {
                self.from_peaks += 1;
                source
            });
            for scanning in [false, true] {
                if scanning && scan.is_some() {
                    continue;
                }
                let reader = match self.reader(api, file, path, size, 2 * index + scanning as usize) {
                    Ok(reader) => reader,
                    Err(reason) => return self.fail(app, reason),
                };
                if scanning {
                    scan = Some(ScanSource::Audio(reader.0));
                } else {
                    if rate != 0 && reader.1 != rate {
                        return self.fail(app, "the take's files differ in sample rate");
                    }
                    (rate, length) = (reader.1, length.min(reader.2));
                    self.play[index] = Some(reader.0);
                }
            }
            scans[index] = scan;
            for channel in 0..usize::from(file.channels).min(8) {
                self.rows[index][channel] = p.row_of(file.first + channel as u8).unwrap_or(0) as u8;
            }
        }
        (p.rate, p.length, p.open) = (rate, length, Some((project, take)));
        self.overview.configure(p.row_count, length);
        self.live_columns = 0;
        self.shown_at = 0;
        // SAFETY: still held: the scanner gets its readers, then goes.
        unsafe { *SCANNER.state() = Some(Scanner { readers: scans, rows: self.rows, overview: self.overview }) };
        self.release();
    }

    fn fail(&mut self, app: &mut App, reason: &'static str) {
        self.problem = Some(reason);
        self.play = [const { None }; MAX_FILES];
        // The scanner keeps no readers when it is the drawing thread's to change; a scanner that did not stop is left
        // reading the old take (nothing shows it) and let go.
        if self.thread.is_none() || self.held {
            // SAFETY: held, or no thread.
            unsafe { *SCANNER.state() = None };
        }
        self.release();
        app.playing.open = None;
        app.message.clear();
        let _ = app.message.write_str(reason);
    }

    /// A reader for `file` (decoder or buffer number `which`, kept for later takes): it, its rate and its frames.
    fn reader(&mut self, api: &TapeApi, file: TakeFile, path: Text<MAX_PATH>, size: u64, which: usize) -> Result<(Reader, u32, u64), &'static str> {
        if file.flac {
            if self.decoders[which].is_none() {
                self.decoders[which] = Some(alloc_zeroed(api).ok_or("out of memory for the FLAC decoder")?);
            }
            let decoder: &'static mut Flac = unsafe { &mut *(self.decoders[which].as_deref_mut().unwrap() as *mut Flac) };
            *decoder.source() = FileSource::vfs(path.as_str(), size).ok_or("a file of the take has too long a path")?;
            decoder.open().map_err(|_| "a file of the take is not FLAC the decoder can read")?;
            if decoder.total_samples == 0 {
                return Err("a FLAC file of the take has no length yet (still being recorded?)");
            }
            if decoder.channels != u32::from(file.channels) {
                return Err("a FLAC file's channels differ from its name");
            }
            let (rate, frames, scale) = (decoder.sample_rate, decoder.total_samples, 1.0 / (1u64 << (decoder.bits_per_sample - 1)) as f32);
            Ok((Reader { kind: Kind::Flac(decoder), file, start: 0, end: 0, at: 0, scale }, rate, frames))
        } else {
            // The Recorder writes a WAV header with its first block: a take that has none yet is still starting.
            if size < 64 {
                return Err("this take is being recorded and has nothing on disk yet");
            }
            let slot = which % 2;
            if self.wavs[slot].is_none() {
                self.wavs[slot] = Some(alloc_zeroed(api).ok_or("out of memory for the WAV reader")?);
            }
            let decoder: &'static mut Wav = unsafe { &mut *(self.wavs[slot].as_deref_mut().unwrap() as *mut Wav) };
            *decoder.source() = FileSource::vfs(path.as_str(), size).ok_or("a file of the take has too long a path")?;
            decoder.open().map_err(|_| "a file of the take is not a WAV file the page can read")?;
            if decoder.channels() != u32::from(file.channels) {
                return Err("a WAV file's channels differ from its name");
            }
            let (rate, frames) = (decoder.sample_rate(), decoder.total_samples());
            Ok((Reader { kind: Kind::Wav(decoder), file, start: 0, end: 0, at: 0, scale: 1.0 }, rate, frames))
        }
    }

    fn seek(&mut self, app: &mut App, frame: u64) {
        let frame = frame.min(app.playing.length);
        for reader in self.play.iter_mut().flatten() {
            reader.seek(frame);
        }
        app.playing.position = frame;
    }

    /// Starts (or restarts, after a seek) counting what is played from the take's position.
    fn start(&mut self, api: &TapeApi, app: &mut App) {
        if !app.playing.available {
            app.message.clear();
            let _ = app.message.write_str("playback is not available (the output is in use)");
            return;
        }
        let mut info = AudioInfo::default();
        (api.audio_info)(&mut info);
        // What is queued now plays out first (at most `AHEAD_MS`); reading goes on from the position shown, which is
        // behind the readers by what was queued when it paused.
        self.room = self.room.max(info.playback_space);
        let position = app.playing.position;
        self.seek(app, position);
        (self.written, self.started) = (0, position);
        app.playing.state = PlayState::Playing;
    }

    /// Mixes and writes until playback is `AHEAD_MS` ahead; the shown position is what the device has played.
    fn pump(&mut self, api: &TapeApi, app: &mut App) -> bool {
        let p = &mut app.playing;
        let (Some(output), true) = (p.output, p.available) else { return false };
        let frame_bytes = self.channels * self.slot;
        let mut info = AudioInfo::default();
        (api.audio_info)(&mut info);
        self.room = self.room.max(info.playback_space);
        let queued = u64::from(self.room - info.playback_space.min(self.room)) / frame_bytes.max(1) as u64;
        let ahead = u64::from(p.rate) * AHEAD_MS / 1000;
        p.position = (self.started + self.written.saturating_sub(queued)).min(p.length);
        if queued >= ahead {
            return true;
        }
        let gains = p.gains();
        let mut wanted = (ahead - queued) as usize;
        let mut bytes = [0u8; STEP * 8 * 4];
        let mut sides = [[0f32; STEP]; 2];
        while wanted > 0 {
            let at = self.started + self.written;
            let step = wanted.min(STEP).min(bytes.len() / frame_bytes.max(1)).min(p.length.saturating_sub(at) as usize);
            if step == 0 {
                // The end of the take: what is queued plays out, then it stops at the end.
                if queued == 0 {
                    p.state = PlayState::Stopped;
                    p.position = p.length;
                }
                break;
            }
            let mut made = 0;
            while made < step {
                let mut count = step - made;
                for reader in self.play.iter_mut().flatten() {
                    count = count.min(reader.ready());
                }
                if count == 0 {
                    break;
                }
                for index in 0..count {
                    let mut frame = [0f32; MAX_ROWS];
                    for (file, reader) in self.play.iter().enumerate() {
                        if let Some(reader) = reader {
                            for channel in 0..usize::from(reader.file.channels).min(8) {
                                frame[usize::from(self.rows[file][channel])] = reader.sample(channel, reader.start + index);
                            }
                        }
                    }
                    let (left, right) = Playing::mix(&gains, &frame);
                    sides[0][made + index] = left;
                    sides[1][made + index] = right;
                }
                for reader in self.play.iter_mut().flatten() {
                    reader.take(count);
                }
                made += count;
            }
            if made == 0 {
                p.state = PlayState::Stopped;
                break;
            }
            for index in 0..made {
                let frame = &mut bytes[index * frame_bytes..(index + 1) * frame_bytes];
                frame.fill(0);
                for (side, channel) in [(0, output), (1, output + 1)] {
                    if channel < self.channels {
                        put_sample(&mut frame[channel * self.slot..(channel + 1) * self.slot], sides[side][index], self.float);
                    }
                }
            }
            let mut sent = 0;
            while sent < made * frame_bytes {
                let wrote = (api.audio_write)(bytes[sent..].as_ptr(), made * frame_bytes - sent);
                if wrote == 0 {
                    break;
                }
                sent += wrote;
            }
            let frames = sent / frame_bytes.max(1);
            self.live.push(&sides[0][..frames], &sides[1][..frames]);
            self.written += frames as u64;
            self.played += frames as u64;
            if frames < made {
                // The ring is full after all: the frames not taken are played again from here next pass.
                let back = self.started + self.written;
                for reader in self.play.iter_mut().flatten() {
                    reader.seek(back);
                }
                break;
            }
            wanted -= made;
        }
        true
    }

}

/// What the overview reads of a file: its peak file (`kakelfix::peaks`, written by the Recorder beside it), or the audio.
enum ScanSource {
    Peaks { path: Text<MAX_PATH>, header: peaks::Header, next: u64, channels: usize },
    Audio(Reader),
}

/// The overview's reader: a second reader of each file, read from start to end into the overview. Owned by the scanner
/// thread except while the drawing thread holds it (`Player::hold`, `tape_rt::channel::Handover`), and by the drawing
/// thread alone when there is no thread.
struct Scanner {
    readers: [Option<ScanSource>; MAX_FILES],
    rows: [[u8; 8]; MAX_FILES],
    overview: &'static Overview,
}

static SCANNER: Handover<Option<Scanner>> = Handover::new(None);

impl Scanner {
    /// Reads `SCAN_FRAMES` more of each file into the overview; false when every file is read through.
    fn step(&mut self) -> bool {
        let mut any = false;
        let mut samples = [0f32; 1024];
        for (file, source) in self.readers.iter_mut().enumerate() {
            let reader = match source {
                Some(ScanSource::Audio(reader)) => reader,
                Some(ScanSource::Peaks { path, header, next, channels }) => {
                    // A piece of the peak file: every peak of it spans its columns.
                    let mut bytes = [0u8; 16 * 1024];
                    let cell = 4 * *channels;
                    let count = ((header.peaks - *next) as usize).min(bytes.len() / cell);
                    let got = if count == 0 { 0 } else { tape_rt::vfs_read(path.as_str(), header.offset(*next), &mut bytes[..count * cell]).unwrap_or(0) / cell };
                    let per = u64::from(header.frames_per_peak);
                    for peak in 0..got {
                        let at = (*next + peak as u64) * per;
                        let frames = per.min(header.frames.saturating_sub(at));
                        for channel in 0..*channels {
                            let word = u32::from_le_bytes(bytes[peak * cell + channel * 4..][..4].try_into().unwrap_or([0; 4]));
                            let (low, high) = peaks::unpack(word);
                            self.overview.push_range(usize::from(self.rows[file][channel]), at, frames, low, high);
                        }
                    }
                    *next += got as u64;
                    if got == 0 {
                        // Read through (or a read failed): its rows count as read to the end.
                        for channel in 0..*channels {
                            self.overview.push_range(usize::from(self.rows[file][channel]), self.overview.frames(), 0, 0, 0);
                        }
                    }
                    any |= got > 0;
                    continue;
                }
                None => continue,
            };
            let mut moved = false;
            let mut left = SCAN_FRAMES;
            while left > 0 {
                let ready = reader.ready().min(left).min(samples.len());
                if ready == 0 {
                    break;
                }
                let at = reader.position();
                for channel in 0..usize::from(reader.file.channels).min(8) {
                    for (index, sample) in samples[..ready].iter_mut().enumerate() {
                        *sample = reader.sample(channel, reader.start + index);
                    }
                    self.overview.push(usize::from(self.rows[file][channel]), at, &samples[..ready]);
                }
                reader.take(ready);
                left -= ready;
                moved = true;
            }
            any |= moved;
            if !moved {
                // A file that ends early (or fails): its rows count as read to the end, so the scan finishes.
                for channel in 0..usize::from(reader.file.channels).min(8) {
                    self.overview.push(usize::from(self.rows[file][channel]), self.overview.frames(), &[]);
                }
            }
        }
        any
    }
}

/// The scanner thread: reads while there is a take to read, rests between steps so the drawing thread keeps its CPU,
/// holds off when asked, sleeps when done, ends at `quit` (`Handover::run`).
extern "C" fn scanner_thread(_: usize) {
    SCANNER.run(|scanner| scanner.as_mut().is_some_and(Scanner::step));
}

/// The peak file beside `file` in `folder`, when there is one that fits it (its channels; the Recorder writes one when a
/// take stops).
fn peak_source(folder: &str, file: TakeFile) -> Option<ScanSource> {
    let leaf = naming::peak_file(file.name.as_str())?;
    let path = naming::join(&[folder, leaf.as_str()]);
    let size = tape_rt::vfs_stat(path.as_str()).ok()?.size;
    let mut head = [0u8; peaks::HEADER];
    let got = tape_rt::vfs_read(path.as_str(), 0, &mut head).ok()?;
    let header = peaks::Header::parse(&head[..got], size)?;
    (header.channels == u32::from(file.channels)).then_some(ScanSource::Peaks { path, header, next: 0, channels: usize::from(file.channels) })
}

/// Writes `value` (-1..1) into a playback slot: a float, or an integer left-justified in the slot's bytes.
fn put_sample(slot: &mut [u8], value: f32, float: bool) {
    let value = value.clamp(-1.0, 1.0);
    if float && slot.len() == 4 {
        slot.copy_from_slice(&value.to_le_bytes());
        return;
    }
    let word = ((value * 2_147_483_647.0) as i32).to_le_bytes();
    let width = slot.len().min(4);
    slot[..width].copy_from_slice(&word[4 - width..]);
}

/// `ext1/recordings`: where the projects are on the Play page's disk (the Recorder's unless one was picked here).
fn base_path(app: &App) -> Text<MAX_PATH> {
    let prefix = app.play_disk().map_or(Text::new(), |disk| disk.prefix());
    naming::join(&[prefix.as_str(), app.recording.root()])
}

/// Every entry of folder `path`: its name and whether it is a folder.
fn each_entry(path: &str, mut each: impl FnMut(&str, bool)) {
    let mut index = 0;
    while let Ok(Some(entry)) = tape_rt::vfs_read_dir(path, index) {
        index += 1;
        if let Ok(name) = core::str::from_utf8(entry.name()) {
            each(name, entry.stat.kind == VFS_DIR);
        }
    }
}

/// The page: the lists at the left, the take's rows with the play head, what plays now, the transport.
pub fn draw(canvas: &mut Canvas, layout: &Layout, app: &App, player: Option<&Player>) {
    let t = theme();
    let p = &app.playing;
    for focus in [Focus::Projects, Focus::Takes] {
        let title = layout.play_title(focus);
        let mut line = Text::<128>::new();
        let _ = match focus {
            Focus::Projects => write!(line, "Projects ({})", p.project_count),
            Focus::Takes => write!(line, "Takes ({})", p.take_count),
        };
        text(canvas, layout, title.x, title.y, line.as_str(), if p.focus == focus { t.text } else { t.dim });
        let count = if focus == Focus::Projects { p.project_count } else { p.take_count };
        for index in 0..count {
            let Some(item) = layout.play_item(focus, index, app) else { continue };
            let picked = if focus == Focus::Projects { index == p.project } else { index == p.take };
            let name = if focus == Focus::Projects { p.projects[index].as_str() } else { p.takes[index].as_str() };
            let open = focus == Focus::Takes && p.open.is_some_and(|(project, take)| project == p.projects[p.project] && take == p.takes[index]);
            canvas.fill_rect(item.x, item.y, item.w as u32, item.h as u32, if picked { t.grid } else { t.background });
            canvas.rect(item.x, item.y, item.w as u32, item.h as u32, if picked && p.focus == focus { t.text } else { t.frame });
            let y = item.y + (item.h - layout.row) / 2;
            if open {
                text(canvas, layout, item.x + layout.col / 2, y, ">", t.green);
            }
            let room = ((item.w - layout.col * 2) / layout.col).max(1) as usize;
            let shown = name.char_indices().nth(room).map_or(name, |(end, _)| &name[..end]);
            text(canvas, layout, item.x + layout.col * 3 / 2, y, shown, if picked { t.text } else { t.dim });
        }
    }
    // The disk the lists are from, a button.
    let row = layout.play_disk();
    canvas.fill_rect(row.x, row.y, row.w as u32, row.h as u32, t.grid);
    canvas.rect(row.x, row.y, row.w as u32, row.h as u32, t.frame);
    let mut line = Text::<128>::new();
    match app.play_disk() {
        Some(disk) => {
            let _ = write!(line, "Disk    {} {}", disk.name().as_str(), disk.volume.name());
            if !disk.label.is_empty() {
                let _ = write!(line, " @{}", disk.label.as_str());
            }
            let _ = write!(line, " {}", disk.model.as_str().trim());
        }
        None => {
            let _ = line.write_str("Disk    none");
        }
    }
    let room = ((row.w - layout.col * 3) / layout.col).max(1) as usize;
    let shown = line.as_str().char_indices().nth(room).map_or(line.as_str(), |(end, _)| &line.as_str()[..end]);
    let y = row.y + (row.h - layout.row) / 2;
    text(canvas, layout, row.x + layout.col / 2, y, shown, if app.play_disk().is_some() { t.text } else { t.dim });
    text(canvas, layout, row.x + row.w - layout.col * 2, y, ">", t.dim);
    // The output pair, a button.
    let row = layout.play_output();
    canvas.fill_rect(row.x, row.y, row.w as u32, row.h as u32, t.grid);
    canvas.rect(row.x, row.y, row.w as u32, row.h as u32, t.frame);
    let mut line = Text::<128>::new();
    match p.output.filter(|_| p.available) {
        Some(first) => {
            let _ = write!(line, "Output  {} + {}", app.name(Group::Output, first), app.name(Group::Output, first + 1));
        }
        None => {
            let _ = line.write_str("Output  not available");
        }
    }
    let y = row.y + (row.h - layout.row) / 2;
    text(canvas, layout, row.x + layout.col / 2, y, line.as_str(), if p.available { t.text } else { t.dim });
    text(canvas, layout, row.x + row.w - layout.col * 2, y, ">", t.dim);
    let area = layout.play_area();
    let Some(player) = player else {
        return centered(canvas, layout, area, "out of memory for the player", t.dim);
    };
    // The title: the take, where playing is, how long it is, what the overview has read.
    line.clear();
    match p.open {
        Some((project, take)) => {
            let _ = write!(line, "{} / {}   ", project.as_str(), take.as_str());
            clock(&mut line, p.seconds(p.position));
            let _ = line.write_str(" / ");
            clock(&mut line, p.seconds(p.length));
            let progress = player.overview().progress();
            if progress < 1.0 {
                let _ = write!(line, "   reading {}%", (progress * 100.0) as u32);
            }
        }
        None => {
            let _ = line.write_str(player.problem().unwrap_or("pick a take: Enter or a tap opens it, space plays"));
        }
    }
    text(canvas, layout, area.x, area.y, line.as_str(), t.text);
    let wave = layout.play_wave();
    let overview = player.overview();
    for row in 0..p.row_count {
        let Some(band) = layout.play_row(row, app) else { continue };
        let listened = p.listen & 1 << row != 0;
        let colour = if listened { t.green } else { t.dim };
        if row == p.row {
            canvas.fill_rect(band.x, band.y, (wave.x - band.x - 2) as u32, band.h as u32, t.grid);
        }
        let mut name = Text::<16>::new();
        let _ = write!(name, "In {}", p.rows[row] + 1);
        text(canvas, layout, band.x + 2, band.y + (band.h - layout.row) / 2, name.as_str(), colour);
        let (centre, half) = (band.y + band.h / 2, ((band.h / 2) - 1).max(1) as f32);
        canvas.fill_rect(wave.x, band.y + band.h - 1, wave.w as u32, 1, t.grid);
        let columns = overview.columns().max(1);
        for x in 0..wave.w {
            let column = (x as usize * columns / wave.w.max(1) as usize).min(columns - 1);
            if let Some((low, high)) = overview.column(row, column) {
                let (top, bottom) = (centre - (high * half) as i32, centre - (low * half) as i32);
                canvas.fill_rect(wave.x + x, top, 1, (bottom - top + 1).max(1) as u32, colour);
            }
        }
    }
    if p.open.is_some() && p.length > 0 {
        let x = wave.x + (p.position as f64 / p.length as f64 * f64::from(wave.w)) as i32;
        canvas.fill_rect(x.min(wave.x + wave.w - 1), wave.y, 2, wave.h as u32, t.amber);
    }
    // What plays now: the two sides, the newest at the right.
    let live = layout.play_live();
    let envelope = player.live();
    for lane in 0..2 {
        let area = Rect::new(live.x, live.y + lane * live.h / 2, live.w, live.h / 2);
        let (centre, half) = (area.y + area.h / 2, (area.h / 2 - 1).max(1) as f32);
        canvas.rect(area.x, area.y, area.w as u32, area.h as u32, t.grid_major);
        canvas.fill_rect(area.x, centre, area.w as u32, 1, t.grid);
        for age in 0..envelope.filled().min(area.w as usize) {
            let [low, high] = envelope.back(age)[lane as usize];
            let (top, bottom) = (centre - (high.clamp(-1.0, 1.0) * half) as i32, centre - (low.clamp(-1.0, 1.0) * half) as i32);
            canvas.fill_rect(area.x + area.w - 1 - age as i32, top, 1, (bottom - top + 1).max(1) as u32, t.curves[lane as usize]);
        }
        text(canvas, layout, area.x + 4, area.y + 2, if lane == 0 { "L" } else { "R" }, t.dim);
    }
    for button in PlayButton::ALL {
        transport(canvas, layout, button, p.state == PlayState::Playing);
    }
}

/// A transport button: framed, its symbol drawn (a triangle, bars, a square) or written.
fn transport(canvas: &mut Canvas, layout: &Layout, button: PlayButton, playing: bool) {
    let t = theme();
    let area = layout.play_button(button);
    let lit = button == PlayButton::Play && playing;
    canvas.fill_rect(area.x, area.y, area.w as u32, area.h as u32, t.grid);
    canvas.rect(area.x, area.y, area.w as u32, area.h as u32, if lit { t.green } else { t.frame });
    let (cx, cy, size) = (area.x + area.w / 2, area.y + area.h / 2, area.h / 3);
    match button {
        PlayButton::Play if playing => {
            canvas.fill_rect(cx - size / 2, cy - size / 2, (size / 3).max(2) as u32, size as u32, t.green);
            canvas.fill_rect(cx + size / 6, cy - size / 2, (size / 3).max(2) as u32, size as u32, t.green);
        }
        PlayButton::Play => {
            // A triangle pointing right, one column at a time.
            for dx in 0..size {
                let h = size - dx * size / size.max(1);
                canvas.fill_rect(cx - size / 2 + dx, cy - h / 2, 1, h.max(1) as u32, t.green);
            }
        }
        PlayButton::Stop => canvas.fill_rect(cx - size / 2, cy - size / 2, size as u32, size as u32, t.text),
        PlayButton::Start => centered(canvas, layout, area, "|<", t.text),
        PlayButton::Back => centered(canvas, layout, area, "<< 10s", t.text),
        PlayButton::Forward => centered(canvas, layout, area, "10s >>", t.text),
    }
}

/// `m:ss` or `h:mm:ss`.
fn clock<const N: usize>(out: &mut Text<N>, seconds: u64) {
    let _ = if seconds >= 3600 {
        write!(out, "{}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        write!(out, "{}:{:02}", seconds / 60, seconds % 60)
    };
}
