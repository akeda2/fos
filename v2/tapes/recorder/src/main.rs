//! `recorder`: a field recorder for the USB audio interface.
//!
//! ```text
//! tape run recorder                          meters and a pre-roll buffer at once; R starts a take
//! tape run recorder name=gig pre=10 auto     10 s of pre-roll, take starts on sound and ends on silence
//! tape run recorder rate=96000 thresh=-45 sound=200 silence=8000
//! tape run recorder tone=440 gate=2,4        a test tone, on for 2 s and off for 4 s, instead of the device
//! tape run recorder disk2/gig                files go to another volume (`@LABEL/gig` works too)
//! ```
//!
//! Capture starts when the tape does: the meters work, and the last `pre` seconds (default 5) of audio are kept in
//! memory, so a take can begin *before* the key was pressed or the sound began, as on a handheld recorder. A take is
//! a WAV file named `NAME001.wav`, `NAME002.wav`, ... (the next free number), written to disk as it arrives; when
//! the volume cannot hold a file any bigger the take continues in the next one without losing a sample.
//!
//! Keys: Space or R starts and stops a take, A arms or disarms the silence trigger (sound starts a take, silence of
//! `silence` ms ends it, sound has to last `sound` ms and exceed `thresh` dBFS), N/B page the meters, U changes the
//! scale, H clears the peak holds, Q or Escape leaves.
#![no_std]
#![no_main]

use audio_rec::peaks;
use audio_rec::preroll::PreRoll;
use audio_rec::recorder::Recorder;
use audio_rec::stream::StreamSink;
use audio_rec::silence::{Config, Detector, Event};
use audio_rec::wav::Format;
use audio_wire::meter::{Coefficients, Ppm};
use core::fmt::Write;
use tape_panel::meters::MeterBank;
use tape_panel::{Text, at, draw};
use tape_rt::{AudioInfo, AudioStats, DISK_EXFAT, DISK_FAT32, DISK_FOSFS, DiskInfo, FORMAT_BOOT_DISK, FS_NOT_FOUND, TapeApi, println};

const MAX_CHANNELS: usize = 32;
const PAGE_ROWS: usize = 12;
const ROW_SCALE: usize = 8;
const ROW_FIRST: usize = 10;
const HINT: &str = "Space: record  A: auto  D: disk  F: name  X: format  U: scale  H: holds  Q: quit";
/// Bytes written to disk at a time: a multiple of the sector size, small enough that one disk write takes well under
/// the capture ring's headroom even on a slow USB stick.
const CHUNK: usize = 256 << 10;
/// Audio is taken from the device this much at a time.
const BLOCK: usize = 64 << 10;

static mut DETECTORS: [Ppm; MAX_CHANNELS] = [Ppm::new(); MAX_CHANNELS];
static mut CLIPS: [bool; MAX_CHANNELS] = [false; MAX_CHANNELS];
static mut METERS: MeterBank<MAX_CHANNELS> = MeterBank::new(ROW_SCALE, ROW_FIRST, PAGE_ROWS);
static mut BLOCK_BUFFER: [u8; BLOCK] = [0; BLOCK];
static mut SINK: Option<StreamSink> = None;

struct Options<'a> {
    name: &'a str,
    rate: u32,
    pre_seconds: u32,
    silence: Config,
    auto: bool,
    tone: Option<u32>,
    gate: Option<(u32, u32)>,
}

fn parse(text: &str) -> Option<Options<'_>> {
    let mut options = Options { name: "take", rate: 48_000, pre_seconds: 5, silence: Config::DEFAULT, auto: false, tone: None, gate: None };
    for word in text.split_ascii_whitespace() {
        if word == "auto" {
            options.auto = true;
            continue;
        }
        let (key, value) = match word.split_once('=') {
            Some(pair) => pair,
            None => ("name", word), // a bare word is the name
        };
        match key {
            "name" => options.name = value,
            "rate" => options.rate = value.parse().ok()?,
            "pre" => options.pre_seconds = value.parse().ok()?,
            "thresh" => options.silence.threshold_tenths = value.parse::<i32>().ok()? * 10,
            "sound" => options.silence.sound_ms = value.parse().ok()?,
            "silence" => options.silence.silence_ms = value.parse().ok()?,
            "tone" => options.tone = Some(value.parse().ok()?),
            "gate" => {
                let (on, off) = value.split_once(',')?;
                options.gate = Some((on.parse().ok()?, off.parse().ok()?));
            }
            _ => return None,
        }
    }
    Some(options)
}

/// A parabola per half cycle standing in for a sine (integer only), a quarter of full scale, 16 bits.
fn tone_sample(phase: u32) -> i16 {
    let half = i64::from(phase & 0x7fff);
    let magnitude = (half * (0x8000 - half)) >> 13;
    (if phase & 0x8000 != 0 { -magnitude } else { magnitude } / 4) as i16
}

macro_rules! say {
    ($app:expr, $($argument:tt)*) => {{
        let mut text = Text::<100>::new();
        let _ = write!(text, $($argument)*);
        $app.message = text;
    }};
}

const MAX_DISKS: usize = 8;

/// A disk the files can go to, for the `D` menu.
#[derive(Clone, Copy)]
struct Choice {
    number: usize,
    kind: u32,
    sectors: u64,
    /// Free bytes in all and in the longest run (`u64::MAX`: not known).
    free: u64,
    largest: u64,
    boot: bool,
    model: Text<24>,
    label: Text<16>,
}

impl Choice {
    const EMPTY: Choice = Choice { number: 0, kind: 0, sectors: 0, free: u64::MAX, largest: u64::MAX, boot: false, model: Text::new(), label: Text::new() };

    fn describe(&self, out: &mut Text<100>) {
        let kind = match self.kind {
            DISK_FOSFS => "FOSFS",
            DISK_FAT32 => "FAT32",
            DISK_EXFAT => "exFAT",
            _ => "no volume",
        };
        let megabytes = self.sectors / 2048;
        let _ = write!(out, "disk{}  {kind}  ", self.number);
        if megabytes >= 10_240 {
            let _ = write!(out, "{} GiB", megabytes >> 10);
        } else {
            let _ = write!(out, "{megabytes} MiB");
        }
        let _ = write!(out, "  {}", self.model.as_str().trim());
        if !self.label.is_empty() {
            let _ = write!(out, "  @{}", self.label.as_str());
        }
        if self.boot {
            let _ = write!(out, "  (boot disk)");
        }
    }
}

/// `12.3 GiB` / `850 MiB`.
fn size_text(bytes: u64, out: &mut Text<100>) {
    if bytes >= 10 << 30 {
        let _ = write!(out, "{} GiB", bytes >> 30);
    } else if bytes >= 1 << 30 {
        let tenths = (bytes * 10) >> 30;
        let _ = write!(out, "{}.{} GiB", tenths / 10, tenths % 10);
    } else {
        let _ = write!(out, "{} MiB", bytes >> 20);
    }
}

/// The disks with a volume to write to (FOSFS, FAT32 or exFAT), in disk order; the count of them.
fn find_disks(api: &TapeApi, choices: &mut [Choice; MAX_DISKS]) -> usize {
    // A kernel older than this tape has no `disk_info`: the table is shorter.
    if (api.size as usize) < core::mem::size_of::<TapeApi>() {
        return 0;
    }
    let mut count = 0;
    for number in 1..=16 {
        let mut info = DiskInfo::default();
        if (api.disk_info)(number, &mut info) == 0 {
            break;
        }
        if count == MAX_DISKS {
            continue;
        }
        let mut model = Text::<24>::new();
        let mut label = Text::<16>::new();
        for &byte in info.model.iter().take(24).take_while(|&&byte| byte != 0) {
            model.push(if (0x20..0x7f).contains(&byte) { byte } else { b'?' });
        }
        for &byte in info.label.iter().take_while(|&&byte| byte != 0) {
            label.push(byte);
        }
        choices[count] = Choice { number, kind: info.kind, sectors: info.sectors, free: info.free_bytes, largest: info.largest_bytes, boot: info.boot != 0, model, label };
        count += 1;
    }
    count
}

/// Everything a running recorder has besides the pre-roll ring (which is passed in beside it, so that the ring's
/// audio can be read while the take is written).
struct App {
    api: &'static TapeApi,
    /// The file name before the number (`take` for take001.wav) and where the files go: a volume prefix the user typed
    /// (`@label/`, which the disk menu leaves alone) or one of the disks in `choices`.
    base: Text<12>,
    /// The file name being typed (`F`), if the user is editing it.
    editing: Option<Text<12>>,
    /// `X` was pressed: the number of the disk waiting for its confirming keypress.
    confirming: Option<usize>,
    custom_prefix: &'static str,
    choices: [Choice; MAX_DISKS],
    choice_count: usize,
    selected: usize,
    format: Format,
    frame: usize,
    staging: usize,
    take: Option<Recorder<'static, StreamSink>>,
    name: Text<40>,
    next_number: u32,
    /// Bytes in the take's finished files (a split closes one and opens the next), and how many files so far.
    done_bytes: u64,
    files: u32,
    next_checkpoint: u64,
    message: Text<100>,
    errors: u32,
    last_take_frames: u64,
}

impl App {
    /// The volume part of a file reference: `diskN/` for the chosen disk, the user's own prefix, or nothing (the boot volume).
    fn prefix(&self, out: &mut Text<40>) {
        if !self.custom_prefix.is_empty() {
            let _ = write!(out, "{}", self.custom_prefix);
        } else if self.choice_count > 0 {
            let _ = write!(out, "disk{}/", self.choices[self.selected].number);
        }
    }

    /// Asks the kernel again what the chosen disk has free (after a take, or when it was just chosen).
    fn refresh_free(&mut self) {
        if self.choice_count == 0 || (self.api.size as usize) < core::mem::size_of::<TapeApi>() {
            return;
        }
        let number = self.choices[self.selected].number;
        let mut info = DiskInfo::default();
        if (self.api.disk_info)(number, &mut info) != 0 {
            self.choices[self.selected].kind = info.kind;
            self.choices[self.selected].sectors = info.sectors;
            self.choices[self.selected].free = info.free_bytes;
            self.choices[self.selected].largest = info.largest_bytes;
        }
    }

    fn staging(&self) -> &'static mut [u8] {
        unsafe { core::slice::from_raw_parts_mut(self.staging as *mut u8, CHUNK) }
    }

    /// Opens the next unused `BASEnnn.wav`.
    fn open_file(&mut self) -> bool {
        let mut found = false;
        for number in self.next_number..self.next_number + 1000 {
            self.name.clear();
            let mut prefix = Text::<40>::new();
            self.prefix(&mut prefix);
            let _ = write!(self.name, "{}{}{number:03}.wav", prefix.as_str(), self.base.as_str());
            let status = (self.api.fs_read)(self.name.as_str().as_ptr(), self.name.len(), core::ptr::null_mut(), 0);
            if status == FS_NOT_FOUND {
                self.next_number = number + 1;
                found = true;
                break;
            }
        }
        if !found {
            say!(self, "No free file name for {}nnn.wav.", self.base.as_str());
            return false;
        }
        let sink = sink();
        match Recorder::begin(sink, self.staging(), self.format, self.name.as_str().as_bytes(), 0) {
            Ok(recorder) => {
                self.take = Some(recorder);
                self.files += 1;
                true
            }
            Err(error) => {
                say!(self, "Cannot create {} (code {}): the volume may be full or the name invalid.", self.name.as_str(), error.0);
                self.errors += 1;
                false
            }
        }
    }

    /// Begins a take with `lead` bytes of the pre-roll ring ahead of the live audio.
    fn start(&mut self, ring: &PreRoll, lead: usize, now: u64) {
        if self.choice_count > 0 && self.custom_prefix.is_empty() && self.choices[self.selected].kind == 0 {
            say!(self, "disk{} has no volume to record to: X formats it as exFAT, D chooses another disk.", self.choices[self.selected].number);
            return;
        }
        self.done_bytes = 0;
        self.files = 0;
        if !self.open_file() {
            return;
        }
        self.next_checkpoint = now + 2_000_000;
        let (older, newer) = ring.slices(lead);
        self.push(older);
        self.push(newer);
        let mut text = Text::<100>::new();
        let _ = write!(text, "Recording {} (with {} ms of pre-roll).", self.name.as_str(), (older.len() + newer.len()) as u64 / self.frame as u64 * 1000 / u64::from(self.format.rate));
        self.message = text;
    }

    /// Appends audio to the take, continuing in a new file when the volume cannot make this one any bigger.
    fn push(&mut self, mut data: &[u8]) {
        while !data.is_empty() {
            let Some(recorder) = self.take.as_mut() else { return };
            match recorder.push(data) {
                Ok(taken) => data = &data[taken..],
                Err(error) => {
                    say!(self, "Writing {} failed (code {}); the take was stopped.", self.name.as_str(), error.0);
                    self.errors += 1;
                    self.stop();
                    return;
                }
            }
            if !data.is_empty() {
                // The file is full: close it and go on in the next, with no audio lost in between.
                self.close_file();
                if !self.open_file() {
                    self.stop();
                    return;
                }
            }
        }
    }

    fn close_file(&mut self) {
        if let Some(recorder) = self.take.take() {
            match recorder.finish() {
                Ok(summary) => {
                    self.done_bytes += summary.bytes;
                    self.last_take_frames += summary.frames;
                }
                Err(error) => {
                    say!(self, "Closing {} failed (code {}).", self.name.as_str(), error.0);
                    self.errors += 1;
                }
            }
        }
    }

    fn stop(&mut self) {
        let was = self.take.is_some();
        self.close_file();
        if was && self.errors == 0 {
            let seconds = self.last_take_frames / u64::from(self.format.rate);
            say!(self, "Saved {} file(s), {} s: last {}.", self.files, seconds, self.name.as_str());
        }
        self.last_take_frames = 0;
        self.refresh_free();
    }

    fn checkpoint(&mut self, now: u64) {
        if now < self.next_checkpoint {
            return;
        }
        self.next_checkpoint = now + 2_000_000;
        if let Some(recorder) = self.take.as_mut() {
            if let Err(error) = recorder.checkpoint() {
                say!(self, "Writing {} failed (code {}); the take was stopped.", self.name.as_str(), error.0);
                self.errors += 1;
                self.stop();
            }
        }
    }
}

fn draw_status(app: &App, ring: &PreRoll, detector: &Detector, auto: bool, options: &Options, now: u64, started: u64) {
    let seconds = |bytes: u64| bytes / app.frame as u64 / u64::from(app.format.rate);
    let mut line = Text::<100>::new();
    match app.take.as_ref() {
        Some(recorder) => {
            let audio = recorder.audio_bytes() + app.done_bytes;
            let s = seconds(audio);
            let _ = write!(line, "RECORDING {}  {}:{:02}:{:02}  {} MiB  (file {})", app.name.as_str(), s / 3600, s / 60 % 60, s % 60, audio >> 20, app.files);
        }
        None => {
            let s = seconds(ring.len() as u64);
            let _ = write!(line, "MONITORING  pre-roll holds {} s  (up {} s)   next: {}{:03}.wav", s, (now - started) / 1_000_000, app.base.as_str(), app.next_number);
        }
    }
    if let Some(typed) = app.editing.as_ref() {
        line.clear();
        let _ = write!(line, "File name: {}_   (letters, digits, _ -; Enter: ok, Esc: cancel)", typed.as_str());
    }
    show(3, line.as_str());
    let mut line = Text::<100>::new();
    if app.choice_count > 0 && app.custom_prefix.is_empty() {
        let _ = write!(line, "Record to: ");
        app.choices[app.selected].describe(&mut line);
        let _ = write!(line, "{}", if app.take.is_some() { "   (locked)" } else { "   (D: change)" });
    } else if !app.custom_prefix.is_empty() {
        let _ = write!(line, "Record to: {}", app.custom_prefix);
    } else {
        let _ = write!(line, "Record to: the boot volume");
    }
    show(2, line.as_str());
    // Row 7: room on the disk. One take needs one contiguous run, so that is what the time is worked out from.
    let mut line = Text::<100>::new();
    if app.choice_count > 0 && app.custom_prefix.is_empty() {
        let choice = &app.choices[app.selected];
        if choice.largest == u64::MAX {
            let _ = write!(line, "Free space: not shown for this kind of volume");
        } else {
            let _ = write!(line, "Free: ");
            size_text(choice.free, &mut line);
            let _ = write!(line, ", longest run ");
            size_text(choice.largest, &mut line);
            let per_second = u64::from(app.format.rate) * app.frame as u64;
            let minutes = choice.largest / per_second.max(1) / 60;
            let _ = write!(line, " = room for {}h{:02}m at this format", minutes / 60, minutes % 60);
        }
    }
    show(7, line.as_str());
    let mut line = Text::<100>::new();
    let _ = write!(
        line,
        "Auto {}: start above {} dB for {} ms, stop below it for {} ms   [{}]",
        if auto { "ON" } else { "off" },
        options.silence.threshold_tenths / 10,
        options.silence.sound_ms,
        options.silence.silence_ms,
        if detector.sounding() { "sound" } else { "quiet" }
    );
    show(4, line.as_str());
    let sink = sink();
    let mut stats = AudioStats::default();
    if options.tone.is_none() {
        (app.api.audio_stats)(&mut stats);
    }
    let mut line = Text::<100>::new();
    let _ = write!(line, "Disk: {} KiB/s, slowest block {} ms   Device: {} transfer errors, {} overruns", sink.kib_per_second(), sink.write_max_us / 1000, stats.capture_errors, stats.capture_overruns);
    show(5, line.as_str());
    show(6, app.message.as_str());
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let Some(options) = parse(arguments) else {
        return println!("Usage: tape run recorder [NAME] [pre=SECONDS] [rate=HZ] [auto] [thresh=DB] [sound=MS] [silence=MS] [tone=HZ [gate=ON,OFF]]");
    };
    let mut info = AudioInfo::default();
    if options.tone.is_none() {
        if (api.audio_info)(&mut info) == 0 {
            return println!("recorder: no USB audio device.");
        }
        if info.capture_channels == 0 {
            return println!("recorder: the device has no inputs.");
        }
    }
    // A test tone stands in for the device as 2 channels of 16 bits.
    let (channels, slot, bits) = if options.tone.is_some() { (2, 2, 16) } else { (info.capture_channels as usize, info.capture_slot_bytes as usize, info.capture_bits as usize) };
    let rate = options.rate;
    let frame = channels * slot;
    let format = Format { channels: channels as u16, rate, slot: slot as u16, bits: bits as u16 };
    // `NAME` may start with a volume (`disk2/gig`, `@label/gig`): a diskN picks that disk in the menu, anything else is
    // used as typed and the menu leaves it alone.
    let (volume_word, file_part) = match options.name.rsplit_once('/') {
        Some((volume, file)) => (volume, file),
        None => ("", options.name),
    };
    let wanted_disk = volume_word.strip_prefix("disk").and_then(|digits| digits.parse::<usize>().ok());
    let custom_prefix: &'static str = if !volume_word.is_empty() && wanted_disk.is_none() {
        // The word stays alive for the whole run (the arguments are), as does the file part below.
        unsafe { core::mem::transmute::<&str, &'static str>(&options.name[..volume_word.len() + 1]) }
    } else {
        ""
    };
    if file_part.len() > 9 {
        return println!("recorder: the name must be 9 characters or fewer (NAMEnnn.wav has to fit 16).");
    }
    if frame == 0 || frame > BLOCK {
        return println!("recorder: unusable capture format.");
    }

    // Memory: the block staging for disk writes and the pre-roll ring, which holds `sound=` ms more than `pre=`
    // seconds so that a take started by the trigger still begins at the sound's onset.
    let staging = (api.alloc)(CHUNK);
    if staging == 0 {
        return println!("recorder: out of memory.");
    }
    let wanted_ms = u64::from(options.pre_seconds) * 1000 + u64::from(options.silence.sound_ms) + 250;
    let mut ring_bytes = (wanted_ms * u64::from(rate) / 1000) as usize * frame;
    let mut ring_address = 0;
    while ring_bytes >= 4096 {
        ring_address = (api.alloc)(ring_bytes);
        if ring_address != 0 {
            break;
        }
        ring_bytes /= 2;
    }
    let ring_memory: &'static mut [u8] = if ring_address != 0 { unsafe { core::slice::from_raw_parts_mut(ring_address as *mut u8, ring_bytes) } } else { &mut [] };
    let mut ring = PreRoll::new(ring_memory, frame);
    let pre_bytes = (u64::from(options.pre_seconds) * u64::from(rate)) as usize * frame;
    let trigger_bytes = ((u64::from(options.pre_seconds) * 1000 + u64::from(options.silence.sound_ms)) * u64::from(rate) / 1000) as usize * frame;

    // Capture starts now, not when a take does: the meters and the pre-roll need it.
    let status = if options.tone.is_some() { 0 } else { (api.audio_start)(rate, 1, 0) };
    if status != 0 {
        (api.free)(staging, CHUNK);
        if ring_address != 0 {
            (api.free)(ring_address, ring_bytes);
        }
        return println!("recorder: cannot start capture at {rate} Hz (code {status}); the device offers {:?}.", &info.rates[..info.rate_count as usize]);
    }

    let (detectors, clips, meters) = unsafe { (&mut *(&raw mut DETECTORS), &mut *(&raw mut CLIPS), &mut *(&raw mut METERS)) };
    let block_buffer = unsafe { &mut *(&raw mut BLOCK_BUFFER) };
    *detectors = [Ppm::new(); MAX_CHANNELS];
    *clips = [false; MAX_CHANNELS];
    unsafe { SINK = Some(StreamSink::new(api.fs_stream_open, api.fs_stream_write, api.fs_stream_close, tape_rt::micros, CHUNK / 2)) };
    meters.clear();
    for channel in 0..channels.min(MAX_CHANNELS) {
        let mut label = Text::<8>::new();
        let _ = write!(label, "In {:>2}", channel + 1);
        meters.add_row(label.as_str());
    }
    let coefficients = Coefficients::for_rate(rate);
    let mut detector = Detector::new(options.silence, rate);
    let mut auto = options.auto;
    let mut app = App {
        api, base: Text::from(file_part), editing: None, confirming: None, custom_prefix, choices: [Choice::EMPTY; MAX_DISKS], choice_count: 0, selected: 0, format, frame, staging, take: None, name: Text::new(), next_number: 1, done_bytes: 0, files: 0, next_checkpoint: 0,
        message: Text::from("Meters and pre-roll are running. Space or R starts a take."), errors: 0, last_take_frames: 0,
    };

    if custom_prefix.is_empty() {
        app.choice_count = find_disks(api, &mut app.choices);
        // The disk asked for by name, else the boot disk, else the first usable one.
        app.selected = (0..app.choice_count).find(|&index| Some(app.choices[index].number) == wanted_disk).or_else(|| (0..app.choice_count).find(|&index| app.choices[index].boot)).unwrap_or(0);
    }

    at(1, 1);
    draw!("\x1b[2J");
    at(1, 2);
    draw!("fos recorder: {channels} channel(s), {bits}-bit in {slot}-byte slots, {rate} Hz, pre-roll {} s{}\x1b[K", options.pre_seconds, if options.tone.is_some() { " (test tone)" } else { "" });
    meters.draw_scale();

    let started = tape_rt::micros();
    let (mut produced, mut phase) = (0u64, 0u32);
    let mut next_status = started;
    let mut next_meters = started;
    let mut quit = false;
    let mut toggle = false;

    while !quit {
        // ---- keys ----
        loop {
            let event = (api.read_key)();
            if event == 0 {
                break;
            }
            if let Some(number) = app.confirming.take() {
                // Any key other than the disk's number cancels.
                if (event & 0xff) as u8 == b'0' + number as u8 && event != 1 {
                    {
                        let mut text = Text::<100>::new();
                        let _ = write!(text, "Formatting disk{number}: audio capture overruns while this runs...");
                        show(6, text.as_str());
                    }
                    let status = (api.disk_format_exfat)(number, b"FOS".as_ptr(), 3);
                    if status == 0 {
                        app.refresh_free();
                        say!(app, "disk{number} is now one exFAT volume (label FOS), readable on a PC.");
                    } else if status == FORMAT_BOOT_DISK {
                        say!(app, "disk{number} is the boot disk and is never formatted.");
                    } else {
                        say!(app, "Formatting disk{number} failed (code {status}).");
                    }
                } else {
                    say!(app, "Format cancelled.");
                }
                continue;
            }
            if let Some(typed) = app.editing.as_mut() {
                // Typing a file name: letters (lower-cased), digits, `_` and `-` are the characters every volume takes.
                let byte = (event & 0xff) as u8;
                if event == 1 {
                    app.editing = None; // Escape cancels
                    say!(app, "File name unchanged.");
                } else if byte == b'\r' || byte == b'\n' {
                    if typed.is_empty() {
                        say!(app, "A file name needs at least one character.");
                    } else {
                        app.base = *typed;
                        app.next_number = 1;
                        app.editing = None;
                        say!(app, "Files will be called {}nnn.wav.", app.base.as_str());
                    }
                } else if byte == 8 || byte == 127 {
                    typed.pop();
                } else if (byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-') && typed.len() < 9 {
                    typed.push(byte.to_ascii_lowercase());
                }
                continue;
            }
            if event == 1 {
                quit = true;
                break;
            }
            match (event & 0xff) as u8 {
                b'x' | b'X' => {
                    if app.take.is_some() {
                        say!(app, "A take is running: stop it (Space) before formatting.");
                    } else if app.choice_count == 0 || !app.custom_prefix.is_empty() || (api.size as usize) < core::mem::size_of::<TapeApi>() {
                        say!(app, "No disk to format here.");
                    } else if app.choices[app.selected].boot {
                        say!(app, "disk{} is the boot disk and is never formatted.", app.choices[app.selected].number);
                    } else {
                        let choice = app.choices[app.selected];
                        let mut size = Text::<100>::new();
                        size_text(choice.sectors * 512, &mut size);
                        say!(app, "ERASE disk{} ({}) as exFAT? Press {} to confirm; any other key cancels.", choice.number, size.as_str(), choice.number);
                        app.confirming = Some(choice.number);
                    }
                }
                b'f' | b'F' => {
                    if app.take.is_some() {
                        say!(app, "A take is running: stop it (Space) before changing the file name.");
                    } else {
                        app.editing = Some(Text::from(app.base.as_str()));
                    }
                }
                b'q' | b'Q' => quit = true,
                b' ' | b'r' | b'R' => toggle = true,
                b'a' | b'A' => {
                    auto = !auto;
                    detector.force(app.take.is_some());
                }
                b'd' | b'D' => {
                    if app.take.is_some() {
                        say!(app, "A take is running: stop it (Space) before changing disk.");
                    } else if app.choice_count > 1 {
                        app.selected = (app.selected + 1) % app.choice_count;
                        app.next_number = 1; // numbering starts again on the new disk
                        app.refresh_free();
                        let mut text = Text::<100>::new();
                        app.choices[app.selected].describe(&mut text);
                        say!(app, "Recording to {}.", text.as_str());
                    } else {
                        say!(app, "There is no other disk with a volume to record to (FOSFS, FAT32 or exFAT).");
                    }
                }
                b'n' | b'N' => meters.next_page(),
                b'b' | b'B' => meters.previous_page(),
                b'u' | b'U' => meters.toggle_scale(),
                b'h' | b'H' => meters.reset_hold(),
                _ => {}
            }
        }
        if quit {
            break;
        }

        // ---- audio ----
        let got = if let Some(frequency) = options.tone {
            // Paced by the clock like a device. With a gate the tone alternates with silence.
            let due = (tape_rt::micros() - started) * u64::from(rate) / 1_000_000;
            let frames = (due - produced).min((BLOCK / frame) as u64) as usize;
            for index in 0..frames {
                let second = (produced + index as u64) / u64::from(rate);
                let on = options.gate.is_none_or(|(on, off)| second % u64::from(on + off).max(1) < u64::from(on));
                let sample = if on { tone_sample(phase) } else { 0 }.to_le_bytes();
                phase = (phase + (u64::from(frequency) * 65_536 / u64::from(rate)) as u32) & 0xffff;
                block_buffer[index * frame..index * frame + 2].copy_from_slice(&sample);
                block_buffer[index * frame + 2..index * frame + 4].copy_from_slice(&sample);
            }
            produced += frames as u64;
            frames * frame
        } else {
            (api.audio_read)(block_buffer.as_mut_ptr(), BLOCK / frame * frame)
        };
        let now = tape_rt::micros();
        if got > 0 {
            let block = &block_buffer[..got];
            let loudest = peaks::feed(block, channels, slot, &coefficients, detectors, clips);
            let event = detector.feed(loudest, (got / frame) as u32);
            let recording = app.take.is_some();
            let (mut start, mut stop, mut lead) = (toggle && !recording, toggle && recording, pre_bytes);
            if auto && !toggle {
                match event {
                    Event::Sound if !recording => (start, lead) = (true, trigger_bytes),
                    Event::Silence if recording => stop = true,
                    _ => {}
                }
            }
            toggle = false;
            if stop {
                app.stop();
                ring.clear();
                detector.force(true); // expect silence next, so a sound that goes on does not restart the take at once
            }
            if start {
                app.start(&ring, lead, now);
                if app.take.is_some() {
                    detector.force(true);
                }
            }
            if app.take.is_some() {
                app.push(block);
                app.checkpoint(now);
            } else {
                ring.push(block);
            }
        } else if !toggle {
            // Nothing to do: let the CPU rest until the next interrupt.
            tape_rt::idle();
        }
        // A key with no audio waiting is handled at the next block (a device that is running delivers one every ms).
        let now = tape_rt::micros();
        if now >= next_meters && meters.rows() > 0 {
            next_meters = now + 25_000;
            if meters.take_footer_dirty() {
                meters.draw_footer(HINT);
            } else {
                meters.update(now, |index| (detectors[index].take_max(), core::mem::take(&mut clips[index])));
            }
        }
        if now >= next_status {
            next_status = now + 250_000;
            draw_status(&app, &ring, &detector, auto, &options, now, started);
        }
    }

    app.stop();
    let (stats_errors, stats_overruns) = {
        let mut stats = AudioStats::default();
        if options.tone.is_none() {
            (api.audio_stats)(&mut stats);
            (api.audio_stop)();
        }
        (stats.capture_errors, stats.capture_overruns)
    };
    (api.free)(staging, CHUNK);
    if ring_address != 0 {
        (api.free)(ring_address, ring_bytes);
    }
    at(meters.footer_row() + 4, 1);
    draw!("\x1b[J");
    println!("recorder: {} file(s) written{}; {} transfer error(s), {} overrun(s).", app.next_number - 1, if app.errors > 0 { " (with errors)" } else { "" }, stats_errors, stats_overruns);
}

/// The sink of the run (set once in `tape_main`).
fn sink() -> &'static mut StreamSink {
    unsafe { (*(&raw mut SINK)).as_mut().unwrap() }
}

fn show(row: usize, text: &str) {
    at(row, 2);
    draw!("{text}\x1b[K");
}
