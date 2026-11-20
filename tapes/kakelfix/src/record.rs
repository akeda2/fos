//! The Recorder page: the take, the list of disks, and the drawing (the take's status at the left, a large peak meter per
//! input at the right with its arm button under it).
//!
//! A take is the armed inputs of every captured block, written either
//! - as FLAC (`archive::FlacFile` over `flac_enc`): each input unpacked to -1..1, rounded to 16, 20 or 24 bits by
//!   `flac_enc::depth::Quantizer` (TPDF dither below 24, plain rounding at 24, which is exact for 24-bit capture and for
//!   float samples on the 24-bit grid), the armed inputs in groups of up to 8 (FLAC's limit), one file each, every file on
//!   its own stream handle (`fs_stream_open2`, at most `FS_STREAMS` = 4, which 32 inputs in groups of 8 fit); or
//! - as WAV through `audio_rec::Recorder` and `fs_stream_*`, the device's samples untouched (float stays float).
//!
//! The take is fed from `analyze::Capture`'s one read loop, so it keeps running while another page is shown; capture
//! only stops when no page needs it and no take runs. Encoding and disk writes are made inline on the drawing thread, as
//! the `recorder` tape writes: the kernel's capture ring (asked for large at startup, `audio_set_capture_ring`) absorbs a
//! write or a frame drawn meanwhile, and the page shows how full it got. The encoder runs about 5.8 M samples a second on
//! one core of an i5-6500 (`cargo test --release -p archive -- --ignored encode_speed`): a quarter of a core for 30
//! inputs at 48 kHz.
//!
//! FAT32 is not offered (its 4 GiB file limit is not worth carrying), so a file that fills is a full disk: the take ends.
//!
//! Volumes: FOSFS and exFAT through `fs_stream_*` (`audio_rec::stream::StreamSink`, a reserved contiguous run per file),
//! ext4 through `vfs_*` by path (`audio_rec::vfs::VfsSink`, `ext1/take001.flac`): there a file grows as it is written, and
//! `vfs_reserve` (ABI 42) gives each of a take's files its own region of the volume, so four files written in turn do not
//! alternate extent by extent (which reaches the driver's extent limit after about 170 MiB each).

use crate::analyze::CHUNK;
use crate::meters::Bank;
use crate::view::{centered, text};
use archive::FlacFile;
use audio_rec::recorder::Recorder;
use audio_rec::select;
use audio_rec::recorder::Sink;
use audio_rec::stream::StreamSink;
use audio_rec::vfs::{Files, VfsSink};
use audio_rec::wav::Format;
use audio_screen::Input;
use core::fmt::Write;
use dsp::pcm::unpack_channel;
use flac_enc::Info;
use flac_enc::depth::Quantizer;
use gfx::Canvas;
use gfx::meter::{Scale, draw_bar, draw_scale};
use gfx::theme::theme;
use core::sync::atomic::{AtomicU32, Ordering};
use kakelfix::channel::{Mailbox, Ring};
use kakelfix::recorder::{Disk, Encoding, GROUP, MAX_DISKS, MAX_GROUPS, Request, Take, Volume, clock, describe_channels, duration, size};
use kakelfix::{App, Layout, Rect, Recording, Text, fit_name};
use tape_rt::{AudioBuffer, AudioStats, DISK_EXFAT, DISK_FAT32, DISK_FOSFS, DiskInfo, FS_NOT_FOUND, TapeApi, VFS_FILE, VfsVolume, alloc_bytes, alloc_zeroed};

/// WAV disk writes: 256 KiB blocks (a multiple of the 512-byte sector; the `recorder` tape's size, which real USB sticks
/// and SATA disks take at full speed). FLAC files stage `archive::file::STAGE` (128 KiB) each.
const STAGING: usize = 256 << 10;
/// Headers and file lengths are brought up to date this often, so a power cut keeps the take up to then.
const CHECKPOINT_US: u64 = 2_000_000;
/// What a FLAC file reserves when the disk does not say how long its longest free run is.
const FALLBACK_RESERVE: u64 = 1 << 30;

/// The ext volume's file calls, by path (`tape_rt::vfs_*`).
pub struct TapeFiles;

impl Files for TapeFiles {
    fn create(&mut self, path: &str) -> Result<(), i32> {
        tape_rt::vfs_create(path, VFS_FILE, 0o644, 0, 0).map_err(|code| code as i32)
    }
    fn reserve(&mut self, path: &str, bytes: u64) -> Result<(), i32> {
        tape_rt::vfs_reserve(path, bytes).map_err(|code| code as i32)
    }
    fn write(&mut self, path: &str, offset: u64, data: &[u8]) -> Result<usize, i32> {
        tape_rt::vfs_write(path, offset, data).map_err(|code| code as i32)
    }
    fn truncate(&mut self, path: &str, size: u64) -> Result<(), i32> {
        tape_rt::vfs_truncate(path, size).map_err(|code| code as i32)
    }
    fn sync(&mut self) -> Result<(), i32> {
        tape_rt::vfs_sync().map_err(|code| code as i32)
    }
}

/// Where a file of a take goes: a stream (FOSFS, exFAT) or an ext volume's file by path.
pub enum Out {
    Stream(StreamSink),
    Vfs(VfsSink<TapeFiles>),
}

impl Out {
    fn vfs() -> Out {
        Out::Vfs(VfsSink::new(TapeFiles, tape_rt::micros, archive::file::STAGE / 2))
    }

    /// The big writes so far: bytes, time spent, the slowest one (µs).
    fn speed(&self) -> (u64, u64, u64) {
        match self {
            Out::Stream(sink) => (sink.bytes, sink.write_us, sink.write_max_us),
            Out::Vfs(sink) => (sink.bytes, sink.write_us, sink.write_max_us),
        }
    }
}

impl Sink for Out {
    fn open(&mut self, name: &[u8], reserve: u64) -> Result<u64, i32> {
        match self {
            Out::Stream(sink) => sink.open(name, reserve),
            Out::Vfs(sink) => sink.open(name, reserve),
        }
    }
    fn write(&mut self, offset: u64, data: &[u8], length: u64) -> Result<(), i32> {
        match self {
            Out::Stream(sink) => sink.write(offset, data, length),
            Out::Vfs(sink) => sink.write(offset, data, length),
        }
    }
    fn close(&mut self, length: u64) -> Result<(), i32> {
        match self {
            Out::Stream(sink) => sink.close(length),
            Out::Vfs(sink) => sink.close(length),
        }
    }
}

/// The sink a WAV take writes through. A `Recorder` borrows it for as long as the file is open, so it lives in a static,
/// like the `recorder` tape's; it is set for the take's volume before the file opens.
static mut SINK: Option<Out> = None;

/// One FLAC file of a take: up to 8 armed inputs on their own stream handle (or ext file).
struct Group {
    file: &'static mut FlacFile,
    sink: Out,
    mask: u32,
    quantizers: [Quantizer; GROUP],
}

/// What a take is to be: the Recorder's settings when Record was pressed, and the capture rate. The worker works from this
/// copy, never from the page, which the drawing thread keeps changing.
#[derive(Clone, Copy)]
pub struct Job {
    pub recording: Recording,
    pub rate: u32,
}

/// What the drawing thread asks of the worker.
#[derive(Clone, Copy)]
pub enum Command {
    None,
    /// Start a take; the audio pushed into `RING` from now on belongs to it.
    Start(Job),
    /// End the take after everything pushed before this.
    Stop,
    /// End the take (as `Stop`) and the worker thread.
    Quit,
}

/// What the worker tells the drawing thread: the take as the page shows it, its latest message (a new `said` means a new
/// one), and the last command it has acted on (`COMMANDS` serial).
#[derive(Clone, Copy, Default)]
pub struct Report {
    pub take: Take,
    pub message: Text<72>,
    pub said: u32,
    pub acted: u32,
}

impl Report {
    fn say(&mut self, args: core::fmt::Arguments) {
        self.message.clear();
        let _ = self.message.write_fmt(args);
        self.said = self.said.wrapping_add(1);
    }
}

/// The captured audio of a take, from the drawing thread to the worker (`Ring`); commands one way, reports the other; a
/// counter the worker sleeps on (`futex_wait`) and the drawing thread bumps when it pushes or posts.
static RING: Ring = Ring::new();
static COMMANDS: Mailbox<Command> = Mailbox::new(Command::None);
static REPORTS: Mailbox<Report> = Mailbox::new(Report { take: Take { running: false, frames: 0, bytes: 0, files: 0, file: Text::new(), overruns: 0, backlog_percent: 0, worst_ms: 0, kib_per_second: 0 }, message: Text::new(), said: 0, acted: 0 });
static WORK: AtomicU32 = AtomicU32::new(0);
/// The worker: created on the drawing thread at startup, then used only by the worker thread (or, where the kernel has
/// no threads, by the drawing thread itself through `Feed::pump_inline`).
static mut WORKER: Option<Worker> = None;
/// The ring holds this much captured audio (or less memory if that is all there is): two seconds or more of any
/// interface here. Behind it the kernel's capture ring (up to 256 MiB) holds whatever the ring cannot.
const RING_BYTES: usize = 16 << 20;
/// The worker thread's stack: `FlacFile::begin` builds a ~100 KiB encoder on it.
const WORKER_STACK: usize = 512 << 10;
/// How long quitting waits for the worker to close a take's files.
const QUIT_WAIT_US: u64 = 20_000_000;

pub struct Worker {
    writer: Writer,
    report: Report,
    seen: u32,
    quit: bool,
}

impl Worker {
    /// Acts on a new command, writes what waits in the ring (a capture block at a time), checkpoints, reports. True if
    /// there was anything to do.
    fn pump(&mut self, api: &TapeApi) -> bool {
        let mut busy = false;
        let (command, serial) = COMMANDS.read();
        if serial != self.seen {
            self.seen = serial;
            busy = true;
            match command {
                Command::Start(job) if !self.writer.running => {
                    self.writer.start(api, &job, &mut self.report);
                    if !self.writer.running {
                        RING.clear(); // pushed for a take that did not start
                    }
                }
                Command::Stop | Command::Quit => {
                    busy |= self.drain(api);
                    self.writer.stop(api, &mut self.report);
                    RING.clear();
                    self.quit = matches!(command, Command::Quit);
                }
                _ => {}
            }
            self.report.acted = serial;
        }
        busy |= self.drain(api);
        if self.writer.running {
            self.writer.checkpoint_if_due(api, &mut self.report);
        }
        self.writer.status(api, &mut self.report);
        REPORTS.post(self.report);
        busy
    }

    /// Writes what waits in the ring, at most a capture block at a time (the sample buffers hold `CHUNK` frames).
    fn drain(&mut self, api: &TapeApi) -> bool {
        let most = CHUNK * self.writer.input.frame_bytes().max(1);
        let mut any = false;
        loop {
            let piece = RING.peek();
            if piece.is_empty() {
                return any;
            }
            let length = piece.len().min(most);
            if self.writer.running {
                self.writer.push(api, &piece[..length], &mut self.report);
            }
            RING.take(length);
            any = true;
        }
    }
}

/// The worker thread: pumps while there is work, sleeps on `WORK` otherwise, ends after `Command::Quit`.
extern "C" fn worker_thread(argument: usize) {
    // SAFETY: the table outlives the tape; the worker static is this thread's alone once it runs (`Feed::new`).
    let api = unsafe { &*(argument as *const TapeApi) };
    let Some(worker) = (unsafe { (*(&raw mut WORKER)).as_mut() }) else { return };
    loop {
        let seen = WORK.load(Ordering::Acquire);
        let busy = worker.pump(api);
        if worker.quit {
            return;
        }
        if !busy {
            tape_rt::futex_wait(&WORK, seen);
        }
    }
}

/// The drawing thread's side: posts the page's requests, pushes the captured audio while a take wants it, and copies the
/// worker's reports onto the page.
pub struct Feed {
    thread: Option<tape_rt::Thread>,
    /// The audio goes into the ring: from posting Start until the worker says the take did not start, or Stop is posted.
    feeding: bool,
    started: u32,
    said: u32,
    running: bool,
}

impl Feed {
    /// Builds the worker for capture of `input` (`block`: the most bytes one `audio_read` gives) and, where the kernel runs
    /// tape threads, starts its thread. None when out of memory.
    pub fn new(api: &'static TapeApi, input: &Input, block: usize) -> Option<Feed> {
        let writer = Writer::new(api, input, block)?;
        let ring = alloc_bytes(api, RING_BYTES).or_else(|| alloc_bytes(api, RING_BYTES / 4))?;
        RING.attach(ring, input.frame_bytes());
        // SAFETY: no thread runs yet; from here on only the worker (thread or inline pump) touches the static.
        unsafe { *(&raw mut WORKER) = Some(Worker { writer, report: Report::default(), seen: 0, quit: false }) };
        let thread = if tape_rt::threads_supported() { tape_rt::spawn_thread(worker_thread, api as *const TapeApi as usize, WORKER_STACK).ok() } else { None };
        Some(Feed { thread, feeding: false, started: 0, said: 0, running: false })
    }

    /// Whether encoding runs on a thread of its own (the server kernel) or inline (the embedded kernels).
    pub fn threaded(&self) -> bool {
        self.thread.is_some()
    }

    /// A take is wanted or still being written: capture must go on.
    pub fn taking(&self) -> bool {
        self.feeding || self.running
    }

    fn post(&self, command: Command) -> u32 {
        COMMANDS.post(command);
        WORK.fetch_add(1, Ordering::Release);
        tape_rt::futex_wake(&WORK, 1);
        COMMANDS.serial()
    }

    /// Posts what the page asked for (`Recording::request`).
    pub fn request(&mut self, api: &TapeApi, app: &mut App, rate: u32) {
        match app.recording.request.take() {
            Some(Request::Start) if !self.taking() => {
                self.started = self.post(Command::Start(Job { recording: app.recording, rate }));
                self.feeding = true;
            }
            Some(Request::Stop) => {
                self.post(Command::Stop);
                self.feeding = false;
            }
            _ => {}
        }
        self.pump_inline(api);
    }

    /// Room for a block of `bytes` (when a take is fed): if not, the drawing thread leaves the audio in the kernel's ring.
    pub fn has_room(&self, bytes: usize) -> bool {
        !self.feeding || RING.has_room(bytes)
    }

    /// One captured block, while a take wants it.
    pub fn push(&mut self, api: &TapeApi, data: &[u8]) {
        if !self.feeding {
            return;
        }
        if RING.push(data) {
            WORK.fetch_add(1, Ordering::Release);
            tape_rt::futex_wake(&WORK, 1);
        }
        self.pump_inline(api);
    }

    /// Without a thread the drawing thread is the worker: it writes what it pushed at once.
    fn pump_inline(&mut self, api: &TapeApi) {
        if self.thread.is_none() {
            if let Some(worker) = unsafe { (*(&raw mut WORKER)).as_mut() } {
                worker.pump(api);
            }
        }
    }

    /// Copies the worker's latest report onto the page; when a take has ended, asks the disk what is free.
    pub fn sync(&mut self, api: &TapeApi, app: &mut App) {
        self.pump_inline(api);
        let (report, _) = REPORTS.read();
        if self.feeding && report.acted >= self.started && !report.take.running {
            self.feeding = false; // the take did not start; the message says why
        }
        let was = self.running;
        self.running = report.take.running;
        let r = &mut app.recording;
        r.take = report.take;
        r.take.running = self.feeding || self.running;
        // The ring's fill counts as backlog too: on the server kernel it is the first to grow behind a slow disk.
        let ring = (RING.waiting() * 100 / RING.capacity().max(1)) as u32;
        r.take.backlog_percent = r.take.backlog_percent.max(ring);
        if was && !self.running {
            refresh(api, r.disks[..r.disk_count].get_mut(r.disk));
        }
        if report.said != self.said {
            self.said = report.said;
            app.message = report.message;
        }
    }

    /// At the end of the tape: ends a take (its files closed and complete) and the worker thread.
    pub fn quit(&mut self, api: &TapeApi, app: &mut App) {
        let serial = self.post(Command::Quit);
        self.feeding = false;
        match self.thread.take() {
            Some(thread) => {
                let began = tape_rt::micros();
                while REPORTS.read().0.acted < serial && tape_rt::micros().wrapping_sub(began) < QUIT_WAIT_US {
                    tape_rt::idle();
                }
                thread.join();
            }
            None => self.pump_inline(api),
        }
        self.sync(api, app);
    }
}

pub struct Writer {
    /// The WAV block buffer, as an address: the file's `Recorder` borrows it for as long as it is open.
    staging: usize,
    /// The armed channels of one captured block, picked out of the device's frames (WAV).
    select: &'static mut [u8],
    wav: Option<Recorder<'static, Out>>,
    groups: [Group; MAX_GROUPS],
    /// FLAC files open (0 for a WAV take).
    open_groups: usize,
    /// One channel of a captured block as -1..1, and a group's channels as FLAC samples.
    floats: &'static mut [f32; CHUNK],
    ints: &'static mut [[i32; CHUNK]; GROUP],
    running: bool,
    /// The bits a FLAC take keeps (`Quantizer` gives 24-bit containers; they are shifted down to this).
    bits: u32,
    format: Format,
    mask: u32,
    /// The device's frame: channels, slot bytes, valid bits, float.
    input: Input,
    /// The volume prefix and stem the numbering was found for, and the number to try next.
    numbered: Text<40>,
    next_number: u32,
    next_checkpoint: u64,
    overruns_at_start: u64,
    ring_bytes: u64,
    peak_backlog: u64,
}

impl Writer {
    /// For capture of `input`'s format; `block` is the most bytes one `audio_read` gives (at most `CHUNK` frames). None
    /// when out of memory (about 2 MiB: four FLAC files' buffers).
    pub fn new(api: &TapeApi, input: &Input, block: usize) -> Option<Writer> {
        let staging = (api.alloc)(STAGING);
        if staging == 0 {
            return None;
        }
        // Every field of a `FlacFile` is a plain number, so zeroed memory is one (`begin` sets it up).
        let group = || -> Option<Group> {
            Some(Group {
                file: alloc_zeroed(api)?,
                sink: Out::vfs(),
                mask: 0,
                quantizers: [const { Quantizer::new(24, 1) }; GROUP],
            })
        };
        let groups = [group()?, group()?, group()?, group()?];
        Some(Writer {
            staging,
            select: alloc_bytes(api, block)?,
            wav: None,
            groups,
            open_groups: 0,
            floats: alloc_zeroed(api)?,
            ints: alloc_zeroed(api)?,
            running: false,
            bits: 24,
            format: Format { channels: 0, rate: 0, slot: 0, bits: 0, float: false },
            mask: 0,
            input: Input { ..*input },
            numbered: Text::new(),
            next_number: 1,
            next_checkpoint: 0,
            overruns_at_start: 0,
            ring_bytes: 0,
            peak_backlog: 0,
        })
    }

    fn checkpoint_if_due(&mut self, api: &TapeApi, report: &mut Report) {
        let now = tape_rt::micros();
        if now < self.next_checkpoint {
            return;
        }
        self.next_checkpoint = now + CHECKPOINT_US;
        if let Err(code) = self.checkpoint() {
            report.say(format_args!("writing {} failed (code {code}): take stopped", { report.take.file }.as_str()));
            self.stop(api, report);
        }
    }

    fn checkpoint(&mut self) -> Result<(), i32> {
        if let Some(wav) = self.wav.as_mut() {
            wav.checkpoint().map_err(|e| e.0)?;
        }
        for group in self.groups[..self.open_groups].iter_mut() {
            group.file.checkpoint(&mut group.sink).map_err(code)?;
        }
        Ok(())
    }

    fn start(&mut self, api: &TapeApi, job: &Job, report: &mut Report) {
        let r = &job.recording;
        if let Some(reason) = r.refusal() {
            return report.say(format_args!("{reason}"));
        }
        self.mask = r.armed & r.all();
        let i = &self.input;
        self.format = Format { channels: self.mask.count_ones() as u16, rate: job.rate, slot: i.slot as u16, bits: i.bits as u16, float: i.float };
        self.peak_backlog = 0;
        let mut stats = AudioStats::default();
        (api.audio_stats)(&mut stats);
        self.overruns_at_start = stats.capture_overruns;
        let mut buffer = AudioBuffer::default();
        (api.audio_buffer)(&mut buffer);
        self.ring_bytes = buffer.ring_bytes;
        report.take = Take::default();
        let opened = match r.encoding {
            Encoding::Wav => self.open_wav(api, job, report),
            Encoding::Flac(bits) => self.open_flac(api, job, report, bits),
        };
        if !opened {
            return;
        }
        self.running = true;
        report.take.running = true;
        self.next_checkpoint = tape_rt::micros() + CHECKPOINT_US;
        report.say(format_args!("recording {}", { report.take.file }.as_str()));
    }

    /// The next take number whose files (one per group) are all free on the target volume; its prefix (`disk3/`, `ext1/`).
    fn number(&mut self, api: &TapeApi, job: &Job, report: &mut Report) -> Option<(u32, Text<16>)> {
        let r = &job.recording;
        let disk = r.target()?;
        let prefix = disk.prefix();
        let ext = disk.volume == Volume::Ext;
        let mut numbered = Text::<40>::new();
        let _ = write!(numbered, "{}{}", prefix.as_str(), r.base.as_str());
        if numbered != self.numbered {
            self.numbered = numbered;
            self.next_number = 1;
        }
        let (groups, count) = r.groups();
        // `fs_read` knows FOSFS and FAT; an ext volume's files are asked for with `vfs_stat`.
        let missing = |name: &str| {
            if ext { tape_rt::vfs_stat(name).err() == Some(FS_NOT_FOUND) } else { (api.fs_read)(name.as_ptr(), name.len(), core::ptr::null_mut(), 0) == FS_NOT_FOUND }
        };
        // Every file this take makes, and a single-file take of either format, so a number is never used twice.
        let free = |number: u32| {
            let mut plain = Text::<48>::new();
            let base = r.base.as_str();
            groups[..count].iter().all(|&group| missing(r.file_name(prefix.as_str(), number, group, count).as_str()))
                && [".wav", ".flac"].iter().all(|extension| {
                    plain.clear();
                    let _ = write!(plain, "{}{base}{number:03}{extension}", prefix.as_str());
                    missing(plain.as_str())
                })
        };
        let Some(number) = (self.next_number..self.next_number + 1000).find(|&n| free(n)) else {
            report.say(format_args!("no free file name for {}nnn", r.base.as_str()));
            return None;
        };
        self.next_number = number + 1;
        Some((number, prefix))
    }

    fn open_wav(&mut self, api: &TapeApi, job: &Job, report: &mut Report) -> bool {
        let Some((number, prefix)) = self.number(api, job, report) else { return false };
        let r = &job.recording;
        let name = r.file_name(prefix.as_str(), number, self.mask, 1);
        let ext = r.target().is_some_and(|d| d.volume == Volume::Ext);
        // No `Recorder` holds the static now: the last take's was finished (consumed) before this one.
        let sink = unsafe { (*(&raw mut SINK)).insert(if ext { Out::vfs() } else { Out::Stream(StreamSink::new(api.fs_stream_open, api.fs_stream_write, api.fs_stream_close, tape_rt::micros, STAGING / 2)) }) };
        // Only one `Recorder` holds the staging block at a time: the last one was finished (consumed) before this.
        let staging = unsafe { core::slice::from_raw_parts_mut(self.staging as *mut u8, STAGING) };
        match Recorder::begin(sink, staging, self.format, name.as_str().as_bytes(), 0) {
            Ok(recorder) => {
                self.wav = Some(recorder);
                report.take.file = Text::from(name.as_str());
                report.take.files = 1;
                true
            }
            Err(error) => {
                report.say(format_args!("cannot create {} (code {})", name.as_str(), error.0));
                false
            }
        }
    }

    /// Opens one FLAC file per group. Each reserves its share of the disk's longest free run (by its channel count; FLAC
    /// never needs more than the samples uncompressed), since a stream given 0 would take the whole run and leave the
    /// next none.
    fn open_flac(&mut self, api: &TapeApi, job: &Job, report: &mut Report, bits: u32) -> bool {
        let Some((number, prefix)) = self.number(api, job, report) else { return false };
        let r = &job.recording;
        let (groups, count) = r.groups();
        let largest = r.target().map_or(u64::MAX, |d| d.largest);
        let ext = r.target().is_some_and(|d| d.volume == Volume::Ext);
        let armed = self.mask.count_ones().max(1) as u64;
        self.bits = bits;
        let mut first = Text::<48>::new();
        for (g, &mask) in groups[..count].iter().enumerate() {
            let channels = mask.count_ones();
            let reserve = if largest == u64::MAX { FALLBACK_RESERVE } else { (largest / armed * channels as u64) & !511 };
            let name = r.file_name(prefix.as_str(), number, mask, count);
            let mut inputs = Text::<96>::new();
            describe_channels(mask, 32, &mut inputs);
            let tags = [("ENCODER", "Fosfor OS kakelfix"), ("INPUTS", inputs.as_str())];
            let group = &mut self.groups[g];
            group.sink = if ext { Out::vfs() } else { Out::Stream(StreamSink::with_handle(api.fs_stream_open2, api.fs_stream_write2, api.fs_stream_close2, tape_rt::micros, archive::file::STAGE / 2)) };
            group.mask = mask;
            group.quantizers = core::array::from_fn(|c| Quantizer::new(bits, 0x9E37_79B9u32.wrapping_mul((g * GROUP + c) as u32 + 1)));
            let info = Info { sample_rate: job.rate, channels, bits };
            if let Err(error) = group.file.begin(&mut group.sink, name.as_str().as_bytes(), reserve, info, &tags) {
                report.say(format_args!("cannot create {} ({})", name.as_str(), Problem(error)));
                for opened in self.groups[..g].iter_mut() {
                    opened.file.abandon(&mut opened.sink);
                }
                return false;
            }
            if g == 0 {
                first = name;
            }
        }
        self.open_groups = count;
        report.take.file = Text::from(first.as_str());
        report.take.files = count as u32;
        true
    }

    /// Appends the armed channels of `data` (whole device frames, at most `CHUNK` of them).
    fn push(&mut self, api: &TapeApi, data: &[u8], report: &mut Report) {
        if !self.running {
            return;
        }
        let failed = if self.open_groups > 0 { self.push_flac(data) } else { self.push_wav(data) };
        if let Err(problem) = failed {
            report.say(format_args!("writing {} failed ({problem}): take stopped", { report.take.file }.as_str()));
            self.stop(api, report);
        }
    }

    fn push_wav(&mut self, data: &[u8]) -> Result<(), Problem> {
        let (channels, slot) = (self.input.channels, self.input.slot);
        let full = if channels >= 32 { u32::MAX } else { (1u32 << channels) - 1 };
        let block: &[u8] = if self.mask == full {
            data
        } else {
            let length = select::extract(data, channels, slot, self.mask, self.select);
            &self.select[..length]
        };
        let Some(wav) = self.wav.as_mut() else { return Ok(()) };
        match wav.push(block) {
            Ok(taken) if taken == block.len() => Ok(()),
            Ok(_) => Err(Problem(archive::Error::Full)),
            Err(error) => Err(Problem(archive::Error::Sink(error.0))),
        }
    }

    fn push_flac(&mut self, data: &[u8]) -> Result<(), Problem> {
        let i = &self.input;
        let frames = data.len() / i.frame_bytes().max(1);
        debug_assert!(frames <= CHUNK, "the worker hands over at most a capture block");
        let frames = frames.min(CHUNK);
        let shift = 24 - self.bits;
        for group in self.groups[..self.open_groups].iter_mut() {
            let mut used = 0;
            for channel in (0..32).filter(|&c| group.mask & 1 << c != 0) {
                let got = unpack_channel(data, &mut self.floats[..], i.channels, i.slot, i.bits, i.float, channel).min(frames);
                let (out, quantizer) = (&mut self.ints[used], &mut group.quantizers[used]);
                for (sample, &x) in out[..got].iter_mut().zip(&self.floats[..got]) {
                    *sample = quantizer.quantize(x) >> shift;
                }
                used += 1;
            }
            let ints = &*self.ints;
            let refs: [&[i32]; GROUP] = core::array::from_fn(|c| &ints[c][..if c < used { frames } else { 0 }]);
            group.file.push(&mut group.sink, &refs[..used]).map_err(Problem)?;
        }
        Ok(())
    }

    /// Ends the take: the last blocks, the final headers and lengths.
    fn stop(&mut self, api: &TapeApi, report: &mut Report) {
        if !self.running {
            return;
        }
        self.status(api, report);
        let (mut bytes, mut frames) = (0, 0);
        if let Some(wav) = self.wav.take() {
            match wav.finish() {
                Ok(summary) => (bytes, frames) = (summary.bytes, summary.frames),
                Err(error) => report.say(format_args!("closing {} failed (code {})", { report.take.file }.as_str(), error.0)),
            }
        }
        for group in self.groups[..self.open_groups].iter_mut() {
            match group.file.finish(&mut group.sink) {
                Ok(summary) => {
                    bytes += summary.bytes;
                    frames = summary.samples;
                }
                Err(error) => {
                    group.file.abandon(&mut group.sink);
                    report.say(format_args!("closing a file of {} failed ({})", { report.take.file }.as_str(), Problem(error)));
                }
            }
        }
        self.open_groups = 0;
        self.running = false;
        let t = &mut report.take;
        t.running = false;
        t.frames = frames;
        t.bytes = bytes;
        let seconds = clock(frames, self.format.rate);
        let (files, file) = (t.files, t.file);
        if report.message.as_str().starts_with("recording") || report.message.is_empty() {
            if files > 1 {
                report.say(format_args!("saved {files} files, {}: {}", seconds.as_str(), file.as_str()));
            } else {
                report.say(format_args!("saved {}, {}", file.as_str(), seconds.as_str()));
            }
        }
    }

    /// Brings the take's figures up to date: length, bytes, the disk's speed, lost audio, the kernel's backlog.
    fn status(&mut self, api: &TapeApi, report: &mut Report) {
        if !self.running {
            return;
        }
        let mut stats = AudioStats::default();
        (api.audio_stats)(&mut stats);
        let mut buffer = AudioBuffer::default();
        (api.audio_buffer)(&mut buffer);
        self.peak_backlog = self.peak_backlog.max(buffer.available_bytes);
        let t = &mut report.take;
        // The disk's speed over every file of the take: bytes and time of the big writes, summed.
        let (mut written, mut spent, mut worst) = (0u64, 0u64, 0u64);
        let mut tally = |sink: &Out| {
            let (bytes, us, max) = sink.speed();
            written += bytes;
            spent += us;
            worst = worst.max(max);
        };
        if let Some(wav) = &self.wav {
            let frame = self.format.frame_bytes().max(1);
            t.frames = wav.audio_bytes() / frame;
            t.bytes = wav.audio_bytes();
            if let Some(sink) = unsafe { (*(&raw const SINK)).as_ref() } {
                tally(sink);
            }
        } else if self.open_groups > 0 {
            t.frames = self.groups[0].file.samples();
            t.bytes = self.groups[..self.open_groups].iter().map(|g| g.file.bytes()).sum();
            for group in &self.groups[..self.open_groups] {
                tally(&group.sink);
            }
        }
        t.overruns = stats.capture_overruns.saturating_sub(self.overruns_at_start);
        t.backlog_percent = if self.ring_bytes == 0 { 0 } else { (buffer.available_bytes * 100 / self.ring_bytes) as u32 };
        t.kib_per_second = if spent == 0 { 0 } else { written * 1000 / 1024 / (spent / 1000).max(1) };
        t.worst_ms = worst / 1000;
    }
}

/// An `archive` error as the page says it.
struct Problem(archive::Error);

impl core::fmt::Display for Problem {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            archive::Error::Sink(code) => write!(out, "code {code}"),
            archive::Error::Full => out.write_str("the disk is full"),
            archive::Error::Format => out.write_str("format not possible"),
            archive::Error::NotOpen => out.write_str("not open"),
        }
    }
}

fn code(error: archive::Error) -> i32 {
    match error {
        archive::Error::Sink(code) => code,
        _ => -1,
    }
}

fn volume(kind: u32) -> Volume {
    match kind {
        DISK_FOSFS => Volume::Fosfs,
        DISK_FAT32 => Volume::Fat32,
        DISK_EXFAT => Volume::Exfat,
        _ => Volume::None,
    }
}

fn describe(number: usize, info: &DiskInfo) -> Disk {
    let mut model = Text::<24>::new();
    let printable = |b: &u8| if (0x20..0x7f).contains(b) { *b as char } else { '?' };
    for c in info.model.iter().take(24).take_while(|&&b| b != 0).map(printable) {
        let _ = model.write_char(c);
    }
    let mut label = Text::<16>::new();
    for c in info.label.iter().take_while(|&&b| b != 0).map(printable) {
        let _ = label.write_char(c);
    }
    Disk { number, writable: true, volume: volume(info.kind), boot: info.boot != 0, sectors: info.sectors, free: info.free_bytes, largest: info.largest_bytes, model, label }
}

/// Disk `disk`'s ext volume (`vfs_volume`) as a target of its own: the drive is the disk's, the size and label the volume's.
/// An ext file grows where there is room, so its largest run is taken to be all the free space.
fn describe_ext(disk: &Disk, volume: &VfsVolume) -> Disk {
    let mut label = Text::<16>::new();
    for c in volume.label.iter().take_while(|&&b| b != 0).map(|b| if (0x20..0x7f).contains(b) { *b as char } else { '?' }) {
        let _ = label.write_char(c);
    }
    Disk { volume: Volume::Ext, writable: volume.writable != 0, sectors: volume.total_bytes / 512, free: volume.free_bytes, largest: volume.free_bytes, label, ..*disk }
}

/// Every volume a take could go to (at most `MAX_DISKS`), in disk order: what `disk_info` says of each disk (FOSFS, FAT,
/// or none), then its ext volume if it has one (`vfs_volume`), which takes the place of a "no volume" entry. How many.
pub fn find_disks(api: &TapeApi, disks: &mut [Disk; MAX_DISKS]) -> usize {
    let mut count = 0;
    for number in 1..=16 {
        let mut info = DiskInfo::default();
        if (api.disk_info)(number, &mut info) == 0 {
            break;
        }
        let disk = describe(number, &info);
        let ext = tape_rt::vfs_volume(number).map(|volume| describe_ext(&disk, &volume));
        for entry in [(disk.volume != Volume::None || ext.is_none()).then_some(disk), ext].into_iter().flatten() {
            if count < MAX_DISKS {
                disks[count] = entry;
                count += 1;
            }
        }
    }
    count
}

/// Asks the kernel again what a volume has free (after a take).
fn refresh(api: &TapeApi, disk: Option<&mut Disk>) {
    let Some(disk) = disk else { return };
    let mut info = DiskInfo::default();
    if (api.disk_info)(disk.number, &mut info) == 0 {
        return;
    }
    let drive = describe(disk.number, &info);
    if disk.volume == Volume::Ext {
        if let Some(volume) = tape_rt::vfs_volume(disk.number) {
            *disk = describe_ext(&drive, &volume);
        }
    } else {
        *disk = drive;
    }
}

fn scale(nordic: bool) -> &'static Scale {
    if nordic { &Scale::NORDIC } else { &Scale::DBFS }
}

pub fn draw(canvas: &mut Canvas, layout: &Layout, app: &App, bank: Option<&Bank>, problem: Option<&str>, threaded: bool) {
    if let Some(reason) = problem {
        return centered(canvas, layout, layout.body, reason, theme().dim);
    }
    panel(canvas, layout, app, threaded);
    if let Some(bank) = bank {
        meters(canvas, layout, app, bank);
    }
}

/// The take at the left: a large REC lamp and clock, then the file, the disk, the room left, the format, the warnings.
fn panel(canvas: &mut Canvas, layout: &Layout, app: &App, threaded: bool) {
    let t = theme();
    let r = &app.recording;
    let panel = layout.rec_panel();
    let area = Rect::new(panel.x + 4, panel.y + 8, panel.w - 8, panel.h - 8);
    let big = layout.scale * 3;
    let (big_col, big_row) = (8 * big as i32, 16 * big as i32);
    let running = r.take.running;
    let lamp = if running { t.red } else { t.off };
    canvas.fill_rect(area.x, area.y + big_row / 4, (big_row / 2) as u32, (big_row / 2) as u32, lamp);
    let x = area.x + big_row * 3 / 4;
    canvas.text_scaled(x, area.y, if running { "REC" } else { "STOP" }, big, if running { t.red } else { t.dim });
    let time = clock(r.take.frames, r.rate);
    canvas.text_scaled(x + big_col * 5, area.y, time.as_str(), big, if running { t.text } else { t.dim });
    let mut y = area.y + big_row + layout.row;
    let mut line = Text::<96>::new();
    let mut row = |canvas: &mut Canvas, label: &str, value: &str, colour: u32| {
        text(canvas, layout, area.x, y, label, t.dim);
        let room = ((area.w - layout.col * 8) / layout.col).max(0) as usize;
        let shown = value.char_indices().nth(room).map_or(value, |(end, _)| &value[..end]);
        text(canvas, layout, area.x + layout.col * 8, y, shown, colour);
        y += layout.row + 6;
    };
    // The file: the one being written (the first of a take's files), else what the next take is called.
    let (groups, count) = r.groups();
    if running || r.take.files > 0 {
        let _ = write!(line, "{}", r.take.file.as_str());
        if r.take.files > 1 {
            let _ = write!(line, " and {} more", r.take.files - 1);
        }
    } else {
        // The number goes right after the stem: `take` + `nnn` + `-1-8.flac`.
        let next = r.file_name("", 0, groups[0], count);
        let at = r.base.as_str().len();
        let _ = write!(line, "{}nnn{}", &next.as_str()[..at], &next.as_str()[at + 3..]);
        if count > 1 {
            let _ = write!(line, " and {} more (8 inputs a file)", count - 1);
        }
    }
    row(canvas, if running { "File" } else if r.take.files > 0 { "Last" } else { "Next" }, line.as_str(), t.text);
    line.clear();
    match r.target() {
        Some(disk) => {
            let _ = write!(line, "{} {} {}", disk.name().as_str(), disk.volume.name(), size(disk.sectors * 512).as_str());
            if !disk.label.is_empty() {
                let _ = write!(line, " @{}", disk.label.as_str());
            }
            let _ = write!(line, " {}", disk.model.as_str().trim());
            if disk.boot {
                let _ = line.write_str(" (boot)");
            }
            row(canvas, "Disk", line.as_str(), if r.refusal().is_some() { t.amber } else { t.text });
            line.clear();
            match r.seconds_left() {
                Some(seconds) => {
                    let room = if running { disk.largest.saturating_sub(r.take.bytes) } else { disk.largest };
                    let _ = write!(line, "{}, {} at this format", size(room).as_str(), duration(seconds).as_str());
                    row(canvas, "Room", line.as_str(), if seconds < 600 { t.amber } else { t.text });
                }
                None if r.refusal().is_some() => row(canvas, "Room", r.refusal().unwrap_or(""), t.amber),
                None => row(canvas, "Room", "-", t.dim),
            }
        }
        None => row(canvas, "Disk", "no disk to record to", t.amber),
    }
    line.clear();
    let mut source = Text::<16>::new();
    let _ = if r.float { source.write_str("32-bit float") } else { write!(source, "{}-bit", r.bits) };
    let _ = match r.encoding {
        Encoding::Wav => write!(line, "WAV {} {} Hz", source.as_str(), r.rate),
        Encoding::Flac(bits) => {
            // Dither when the file keeps fewer bits than the capture has (a float device counts as 24).
            let dither = bits < if r.float { 24 } else { r.bits as u32 };
            write!(line, "FLAC {bits}-bit {} Hz{}", r.rate, if dither { ", dithered" } else { "" })
        }
    };
    row(canvas, "Format", line.as_str(), t.text);
    line.clear();
    describe_channels(r.armed & r.all(), r.channels.min(32), &mut line);
    let _ = write!(line, " ({} of {})", r.armed_count(), r.channels);
    row(canvas, "Inputs", line.as_str(), t.text);
    row(canvas, "Writer", if threaded { "own thread (encodes and writes)" } else { "inline (no tape threads here)" }, t.dim);
    if running || r.take.files > 0 {
        line.clear();
        let _ = write!(line, "{}", size(r.take.bytes).as_str());
        if r.take.kib_per_second > 0 {
            let _ = write!(line, ", disk {} KiB/s, slowest {} ms", r.take.kib_per_second, r.take.worst_ms);
        }
        row(canvas, "Written", line.as_str(), t.text);
    }
    // What matters most when audio cannot be taken twice: lost audio, then a disk falling behind.
    line.clear();
    if r.take.overruns > 0 {
        let _ = write!(line, "AUDIO LOST: {} overrun(s), the disk did not keep up", r.take.overruns);
        row(canvas, "", line.as_str(), t.red);
    } else if running && r.take.backlog_percent >= 25 {
        let _ = write!(line, "disk falling behind: {}% of the capture ring waits", r.take.backlog_percent);
        row(canvas, "", line.as_str(), t.amber);
    }
}

/// Large peak meters at the right, one per input (sample peak, or the Nordic PPM), each with its name and arm button.
fn meters(canvas: &mut Canvas, layout: &Layout, app: &App, bank: &Bank) {
    let t = theme();
    let r = &app.recording;
    let scale = scale(r.nordic);
    let area = layout.rec_area();
    let Some(first) = layout.rec_bar(app.first_rec, app) else { return };
    // The scale names itself at both sides; the header says sample peak or Nordic PPM.
    draw_scale(canvas, scale, area.x, layout.rec_span(app) as u32, first.y, first.h as u32);
    for input in 0..r.channels.max(1) {
        let (Some(column), Some(bar), Some(name), Some(arm)) = (layout.rec_column(input, app), layout.rec_bar(input, app), layout.rec_name(input, app), layout.rec_arm(input, app)) else {
            continue;
        };
        let selected = input == r.input;
        if selected {
            canvas.fill_rect(column.x - 2, column.y - 4, (column.w + 4) as u32, (column.h + 4) as u32, t.grid);
        }
        let reading = bank.reading(input, r.nordic);
        let bar_w = (bar.w - 8).min(layout.col * 4);
        draw_bar(canvas, scale, bar.x + (bar.w - bar_w) / 2, bar_w as u32, bar.y, bar.h as u32, reading.level, reading.hold, reading.clipped);
        let label = app.input_name(input);
        let short = fit_name(label.as_str(), (name.w / layout.col).max(1) as usize);
        centered(canvas, layout, name, short.as_str(), if selected { t.text } else { t.dim });
        // The arm button: red when armed (filled while the take runs), dim when not, absent past the 32 a take can hold.
        if !r.armable(input) {
            continue;
        }
        let armed = r.is_armed(input);
        let fill = if armed && r.take.running { t.red } else { t.off };
        canvas.fill_rect(arm.x, arm.y, arm.w as u32, arm.h as u32, fill);
        canvas.rect(arm.x, arm.y, arm.w as u32, arm.h as u32, if armed { t.red } else if selected { t.text } else { t.frame });
        let ink = if armed && r.take.running { t.background } else if armed { t.red } else { t.dim };
        centered(canvas, layout, arm, if armed { "REC" } else { "-" }, ink);
    }
    if app.first_rec > 0 {
        text(canvas, layout, area.x, area.y + area.h - layout.row, "<", t.dim);
    }
    if app.first_rec + layout.visible_rec(app) < r.channels {
        text(canvas, layout, area.x + area.w - layout.col, area.y + area.h - layout.row, ">", t.dim);
    }
}

/// The header's REC lamp, on every page while a take runs: "REC 01:23".
pub fn header_lamp(app: &App) -> Option<Text<24>> {
    let r = &app.recording;
    if !r.take.running {
        return None;
    }
    let mut out = Text::new();
    let _ = write!(out, "REC {}", clock(r.take.frames, r.rate).as_str());
    Some(out)
}
