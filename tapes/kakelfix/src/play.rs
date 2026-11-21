//! The Play page's tape side: the projects and takes on the Recorder's disk (under its root folder), a take's files read
//! through the path calls (`vfs_read`, any volume), FLAC decoded (up to 8 channels a file) or WAV read as it is, the
//! inputs listened to mixed to the output pair (`kakelfix::play::Playing::mix`) and written to the device's playback
//! side, the whole take read through a second time in the background for its overview.
//!
//! Everything runs on the drawing thread, a bounded amount per pass: playback is kept about `AHEAD_MS` ahead of the
//! device, the overview reads `SCAN_FRAMES` a pass. Capture is read in the same loop, so a pass never blocks for long:
//! a disk read is one `vfs_read` of at most `CHUNK` bytes.
//!
//! The stream is started with playback when the device has a playback side (`analyze::Capture`); when it is not (a
//! loopback whose playback end another tape holds), the page says so and only shows takes.

use crate::view::{centered, text};
use audio_decode::flac::Decoder;
use audio_decode::{Source, SourceError};
use core::fmt::Write;
use fosfor_abi::tape::{AudioInfo, TapeApi, VFS_DIR};
use gfx::Canvas;
use gfx::theme::theme;
use kakelfix::analyzer::Envelope;
use kakelfix::naming::{self, MAX_PATH, ProjectName};
use kakelfix::play::{MAX_FILES, MAX_LIST, MAX_ROWS, Overview, TakeFile, TakeName, WavLayout, default_output, take_file};
use kakelfix::{App, Focus, Layout, PlayButton, PlayRequest, PlayState, Playing, Rect, Text};
use mixer_model::Group;
use tape_rt::alloc_zeroed;

/// Playback is kept this far ahead of the device: enough to ride out a slow pass, little enough that a pause, a seek or
/// a stop is heard at once.
const AHEAD_MS: u64 = 150;
/// Frames the overview reads per pass, for each file.
const SCAN_FRAMES: usize = 32_768;
/// The most bytes one disk read asks for.
const CHUNK: usize = 64 * 1024;
/// Frames mixed and written per step.
const STEP: usize = 512;
/// Seconds the live envelope spans.
const LIVE_S: f32 = 4.0;

/// A file of the take, read by path (`vfs_read`: exFAT, ext4, FOSFS alike).
pub struct VfsFile {
    path: Text<MAX_PATH>,
    size: u64,
}

impl Source for VfsFile {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        let length = out.len().min(CHUNK);
        tape_rt::vfs_read(self.path.as_str(), offset, &mut out[..length]).map_err(|_| SourceError)
    }
}

type Flac = Decoder<VfsFile, 8>;

/// Where a reader's frames come from: a FLAC decoder's block, or WAV bytes read in.
enum Kind {
    Flac(&'static mut Flac),
    Wav { layout: WavLayout, bytes: &'static mut WavBuffer, path: Text<MAX_PATH> },
}

/// WAV bytes read in at a time (whole frames of up to 32 four-byte channels).
struct WavBuffer([u8; 4 * MAX_ROWS * 1024]);

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
            Kind::Wav { layout, bytes, .. } => {
                let frame = layout.frame_bytes() as usize;
                layout.sample(&bytes.0[index * frame..index * frame + frame], channel)
            }
        }
    }

    /// Frames ready: refills the block when it is used up. 0 at the end of the file or on a read error.
    fn ready(&mut self) -> usize {
        if self.start < self.end {
            return self.end - self.start;
        }
        self.at += self.end as u64;
        (self.start, self.end) = (0, 0);
        match &mut self.kind {
            Kind::Flac(decoder) => {
                if let Ok(Some(count)) = decoder.next_block() {
                    self.end = count;
                }
            }
            Kind::Wav { layout, bytes, path } => {
                let frame = layout.frame_bytes();
                let left = layout.frames().saturating_sub(self.at);
                let frames = (bytes.0.len() as u64 / frame).min((CHUNK as u64 / frame).max(1)).min(left) as usize;
                let want = frames * frame as usize;
                if want > 0 {
                    if let Ok(got) = tape_rt::vfs_read(path.as_str(), layout.data_offset + self.at * frame, &mut bytes.0[..want]) {
                        self.end = got / frame as usize;
                    }
                }
            }
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
            Kind::Wav { .. } => {
                (self.at, self.start, self.end) = (frame, 0, 0);
            }
        }
    }
}

/// The page's tape side (one, from the heap): readers for playing and for the overview, the overview and the live envelope.
pub struct Player {
    play: [Option<Reader>; MAX_FILES],
    scan: [Option<Reader>; MAX_FILES],
    /// Decoders and WAV buffers made so far (they are kept for the next take; the heap gives nothing back).
    decoders: [Option<&'static mut Flac>; 2 * MAX_FILES],
    buffers: [Option<&'static mut WavBuffer>; 2 * MAX_FILES],
    overview: &'static mut Overview,
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
        player.overview = alloc_zeroed(api)?;
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
            fresh |= self.scan();
        }
        fresh
    }

    /// The projects under the root folder of the Recorder's disk, sorted by name, and the takes of the picked one, the
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
        let p = &mut app.playing;
        if p.project >= p.project_count || p.take >= p.take_count {
            return;
        }
        (p.state, p.open, p.position, p.length) = (PlayState::Stopped, None, 0, 0);
        self.play = [const { None }; MAX_FILES];
        self.scan = [const { None }; MAX_FILES];
        self.problem = None;
        let (project, take) = (p.projects[p.project], p.takes[p.take]);
        let folder = naming::join(&[base_path_of(&app.recording).as_str(), project.as_str(), take.as_str()]);
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
        for index in 0..p.file_count {
            let file = p.files[index];
            let path = naming::join(&[folder.as_str(), file.name.as_str()]);
            let size = tape_rt::vfs_stat(path.as_str()).map(|stat| stat.size).unwrap_or(0);
            for scan in [false, true] {
                let reader = match self.reader(api, file, path, size, 2 * index + scan as usize) {
                    Ok(reader) => reader,
                    Err(reason) => return self.fail(app, reason),
                };
                if !scan {
                    if rate != 0 && reader.1 != rate {
                        return self.fail(app, "the take's files differ in sample rate");
                    }
                    (rate, length) = (reader.1, length.min(reader.2));
                }
                let slot = if scan { &mut self.scan[index] } else { &mut self.play[index] };
                *slot = Some(reader.0);
            }
            for channel in 0..usize::from(file.channels).min(8) {
                self.rows[index][channel] = p.row_of(file.first + channel as u8).unwrap_or(0) as u8;
            }
        }
        (p.rate, p.length, p.open) = (rate, length, Some((project, take)));
        self.overview.configure(p.row_count, length);
        self.live_columns = 0;
    }

    fn fail(&mut self, app: &mut App, reason: &'static str) {
        self.problem = Some(reason);
        self.play = [const { None }; MAX_FILES];
        self.scan = [const { None }; MAX_FILES];
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
            *decoder.source() = VfsFile { path, size };
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
            let mut head = [0u8; 512];
            let got = tape_rt::vfs_read(path.as_str(), 0, &mut head).map_err(|_| "a file of the take cannot be read")?;
            // The Recorder writes a WAV header with its first block: a take that has none yet is still starting.
            if got < 64 {
                return Err("this take is being recorded and has nothing on disk yet");
            }
            let layout = WavLayout::parse(&head[..got], size).ok_or("a file of the take is not a WAV file the page can read")?;
            if layout.channels != u16::from(file.channels) {
                return Err("a WAV file's channels differ from its name");
            }
            if self.buffers[which].is_none() {
                self.buffers[which] = Some(alloc_zeroed(api).ok_or("out of memory for the WAV buffer")?);
            }
            let bytes: &'static mut WavBuffer = unsafe { &mut *(self.buffers[which].as_deref_mut().unwrap() as *mut WavBuffer) };
            Ok((Reader { kind: Kind::Wav { layout, bytes, path }, file, start: 0, end: 0, at: 0, scale: 1.0 }, layout.rate, layout.frames()))
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

    /// Reads the take on for the overview: `SCAN_FRAMES` of each file.
    fn scan(&mut self) -> bool {
        let mut any = false;
        let mut samples = [0f32; 1024];
        for (file, reader) in self.scan.iter_mut().enumerate() {
            let Some(reader) = reader else { continue };
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

/// `ext1/recordings`: where the Recorder's projects are on its disk.
fn base_path(app: &App) -> Text<MAX_PATH> {
    base_path_of(&app.recording)
}

fn base_path_of(r: &kakelfix::Recording) -> Text<MAX_PATH> {
    let prefix = r.target().map_or(Text::new(), |disk| disk.prefix());
    naming::join(&[prefix.as_str(), r.root()])
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
