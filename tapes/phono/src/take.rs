//! Archiving in the phono tape: the raw Lt/Rt take written while listening (`Record`), the second pass that decodes it
//! into a 4.0 / 4.1 / 5.1 FLAC (`Decode`), and the output name both use. The files and the maths are `crates/archive`;
//! this is the glue to the tape ABI and the state the screen shows.
//!
//! The kernel has one file stream, so a take and a decode never run together, and a decode reads the whole raw file into
//! memory (the raw FLAC of one record side is a few hundred MB at most; there is no partial read yet, see DEFERRED.md).

use archive::file::Error as FileError;
use archive::names::{self, Name};
use archive::transcode::{Error as DecodeError, Job};
use archive::{Bytes, FlacFile, Names, Progress, Transcoder};
use audio_decode::flac::Decoder;
use audio_rec::stream::StreamSink;
use core::fmt::Write;
use flac_enc::Info;
use instrument_rt::Output;
use outfx::{Surround, SurroundSettings};
use phono::{CURVES, Phono};
use phono_tape::Setup;
use tape_rt::{FS_NOT_FOUND, Key, Line, TapeApi};

/// Frames of capture that may wait while the disk is busy (seconds of audio), instead of being dropped.
const HEADROOM_SECONDS: usize = 3;
/// The header and file length are brought up to date this often, so a reset keeps the take.
const CHECKPOINT_US: u64 = 2_000_000;
/// The default base name.
pub const DEFAULT_NAME: &str = "lp";

static mut FILE: FlacFile = FlacFile::new();
static mut SINK: Option<StreamSink> = None;
static mut DECODER: Decoder<Bytes<'static>> = Decoder::new(Bytes(&[]));
static mut TRANSCODER: Transcoder = Transcoder::new();
/// The decode pass has its own decoder and equaliser, so what is heard is not touched by it.
static mut PASS_SURROUND: Surround = Surround::new();
static mut PASS_EQ: Option<Phono> = None;

fn file() -> &'static mut FlacFile {
    unsafe { &mut *core::ptr::addr_of_mut!(FILE) }
}

fn sink() -> &'static mut StreamSink {
    unsafe { (*core::ptr::addr_of_mut!(SINK)).as_mut().expect("sink") }
}

/// What the decode pass was started with (settings can change while it runs).
#[derive(Clone, Copy)]
struct DecodeRun {
    buffer: usize,
    size: usize,
    raw: Name,
    out: Name,
    settings: SurroundSettings,
    curve: usize,
}

pub struct Archive {
    api: &'static TapeApi,
    pub name: Line<48>,
    pub editing: Option<Line<48>>,
    pub recording: bool,
    failed: Option<FileError>,
    bits: u32,
    rate: u32,
    next_checkpoint: u64,
    raw: Name,
    last_raw: Option<Name>,
    run: Option<DecodeRun>,
    pub percent: u32,
}

fn say(message: &mut Line<64>, args: core::fmt::Arguments) {
    message.clear();
    let _ = message.write_fmt(args);
}

fn set(line: &mut Line<48>, text: &str) {
    line.clear();
    let _ = line.write_str(text);
}

fn exists(api: &TapeApi, name: &[u8]) -> bool {
    (api.fs_read)(name.as_ptr(), name.len(), core::ptr::null_mut(), 0) != FS_NOT_FOUND
}

/// The length of `name` (`VOLUME/LEAF` or a boot-volume file), found by listing the volume.
fn file_length(api: &TapeApi, name: &str) -> Option<u64> {
    let (volume, leaf) = name.rfind('/').map_or(("", name), |at| (&name[..at], &name[at + 1..]));
    let mut index = 0;
    while let Ok(Some(info)) = tape_rt::list_file(api, volume, index) {
        if info.name[..info.name_length as usize].eq_ignore_ascii_case(leaf.as_bytes()) {
            return Some(info.length);
        }
        index += 1;
    }
    None
}

fn to_int(x: f32, scale: f32) -> i32 {
    let v = x * scale;
    let v = if v >= 0.0 { v + 0.5 } else { v - 0.5 };
    (v as i32).clamp(-(scale as i32), scale as i32 - 1)
}

fn name_error(error: names::NameError, message: &mut Line<64>) {
    match error {
        names::NameError::Empty => say(message, format_args!("name: type a name first")),
        names::NameError::BadChar => say(message, format_args!("name: lowercase letters, digits . _ - (and diskN/ or @LABEL/)")),
        names::NameError::TooLong(most) => say(message, format_args!("name: at most {most} characters here")),
    }
}

impl Archive {
    pub fn new(api: &'static TapeApi, name: &str) -> Archive {
        unsafe { *core::ptr::addr_of_mut!(SINK) = Some(StreamSink::new(api.fs_stream_open, api.fs_stream_write, api.fs_stream_close, tape_rt::micros, 64 * 1024)) };
        let mut line = Line::new();
        set(&mut line, name);
        Archive { api, name: line, editing: None, recording: false, failed: None, bits: 24, rate: 48000, next_checkpoint: 0, raw: Name::new(), last_raw: None, run: None, percent: 0 }
    }

    pub fn decoding(&self) -> bool {
        self.run.is_some()
    }

    /// The raw tap: the captured frames before any equalisation, as integers of the device's depth.
    pub fn tap(&mut self, frames: &[[f32; 2]]) {
        if !self.recording || self.failed.is_some() {
            return;
        }
        let scale = (1u32 << (self.bits - 1)) as f32;
        let (mut left, mut right) = ([0i32; instrument_rt::CHUNK], [0i32; instrument_rt::CHUNK]);
        for chunk in frames.chunks(instrument_rt::CHUNK) {
            for (n, frame) in chunk.iter().enumerate() {
                left[n] = to_int(frame[0], scale);
                right[n] = to_int(frame[1], scale);
            }
            if let Err(error) = file().push(sink(), &[&left[..chunk.len()], &right[..chunk.len()]]) {
                self.failed = Some(error);
                return;
            }
        }
    }

    pub fn start_recording(&mut self, setup: &Setup, output: &mut Output, message: &mut Line<64>) {
        if self.run.is_some() {
            return say(message, format_args!("a decode is running: the disk has one file stream"));
        }
        let bits = output.capture_bits();
        if bits == 0 {
            return say(message, format_args!("no capture to record"));
        }
        let names = match Names::parse(self.name.as_str()) {
            Ok(names) => names,
            Err(error) => return name_error(error, message),
        };
        let api = self.api;
        let Some(number) = names.first_free(|name| exists(api, name)) else {
            return say(message, format_args!("{} 1..99 are all taken", names.base()));
        };
        let raw = names.raw(number);
        let info = Info { sample_rate: output.rate, channels: 2, bits };
        let tags = [("ENCODER", "fosfor phono"), ("CONTENT", "raw Lt/Rt as captured, not equalised"), ("CURVE", CURVES[setup.curve].id), ("TITLE", names.base())];
        if let Err(error) = file().begin(sink(), raw.as_bytes(), 0, info, &tags) {
            return say(message, format_args!("cannot create {}: {}", raw.as_str(), file_error(error)));
        }
        self.recording = true;
        self.failed = None;
        self.bits = bits;
        self.rate = output.rate;
        self.raw = raw;
        self.next_checkpoint = tape_rt::micros() + CHECKPOINT_US;
        output.capture_headroom(HEADROOM_SECONDS * output.rate as usize);
        say(message, format_args!("recording {} ({bits} bits, flat: no EQ in the raw take)", raw.as_str()));
    }

    pub fn stop_recording(&mut self, output: &mut Output, message: &mut Line<64>) {
        if !self.recording {
            return;
        }
        self.recording = false;
        output.capture_headroom(0);
        match file().finish(sink()) {
            Ok(summary) if self.failed.is_none() => {
                self.last_raw = Some(self.raw);
                say(message, format_args!("saved {}: {} min {} s, {} MB", self.raw.as_str(), summary.samples / self.rate as u64 / 60, summary.samples / self.rate as u64 % 60, summary.bytes >> 20));
            }
            Ok(_) | Err(_) => {
                self.last_raw = Some(self.raw);
                let reason = self.failed.take().map_or("write error", file_error);
                say(message, format_args!("{} stopped: {reason}; kept to the last checkpoint", self.raw.as_str()));
            }
        }
    }

    /// Checkpoints a running take, stops a failed one and advances the decode; true when the picture should be redrawn.
    pub fn service(&mut self, output: &mut Output, message: &mut Line<64>) -> bool {
        let mut redraw = false;
        if self.recording {
            if self.failed.is_some() {
                file().abandon(sink());
                self.recording = false;
                output.capture_headroom(0);
                self.last_raw = Some(self.raw);
                let reason = self.failed.take().map_or("write error", file_error);
                say(message, format_args!("{} stopped: {reason}; kept to the last checkpoint", self.raw.as_str()));
                return true;
            }
            let now = tape_rt::micros();
            if now >= self.next_checkpoint {
                self.next_checkpoint = now + CHECKPOINT_US;
                if let Err(error) = file().checkpoint(sink()) {
                    self.failed = Some(error);
                }
            }
            redraw = true;
        }
        if let Some(run) = self.run {
            redraw = true;
            let tags = [("ENCODER", "fosfor phono"), ("DECODER", run.settings.mode.name()), ("LOGIC", run.settings.logic.map_or("off", outfx::Preset::name)), ("SWAP", if run.settings.swap_rear { "on" } else { "off" }), ("CURVE", CURVES[run.curve].id), ("SOURCE", run.raw.as_str())];
            let (decoder, surround, transcoder) = unsafe { (&mut *core::ptr::addr_of_mut!(DECODER), &mut *core::ptr::addr_of_mut!(PASS_SURROUND), &mut *core::ptr::addr_of_mut!(TRANSCODER)) };
            let Some(eq) = (unsafe { (*core::ptr::addr_of_mut!(PASS_EQ)).as_mut() }) else { return true };
            let mut job = Job { file: file(), sink: sink(), eq, name: run.out.as_bytes(), tags: &tags };
            match transcoder.step(decoder, surround, &mut job, 1) {
                Ok(Progress::Working(percent)) => self.percent = percent,
                Ok(Progress::Done(report)) => {
                    let floor = report.floor_dbfs.map_or(0, |db| db as i32);
                    if report.floor_dbfs.is_some() {
                        say(message, format_args!("wrote {}: {} bits (floor {floor} dBFS), {} MB", run.out.as_str(), report.bits, report.bytes >> 20));
                    } else {
                        say(message, format_args!("wrote {}: {} bits, {} MB", run.out.as_str(), report.bits, report.bytes >> 20));
                    }
                    self.end_decode();
                }
                Err(error) => {
                    say(message, format_args!("decode of {} failed: {}", run.raw.as_str(), decode_error(error)));
                    self.end_decode();
                }
            }
        }
        redraw
    }

    /// The newest raw take of this session, else the highest-numbered existing one for the current name.
    fn latest_raw(&self) -> Option<Name> {
        if let Some(raw) = self.last_raw {
            return Some(raw);
        }
        let names = Names::parse(self.name.as_str()).ok()?;
        (1..=names::MAX_NUMBER).map(|n| names.raw(n)).filter(|raw| exists(self.api, raw.as_bytes())).last()
    }

    pub fn start_decode(&mut self, setup: &Setup, message: &mut Line<64>) {
        if self.recording {
            return say(message, format_args!("stop the take first: the disk has one file stream"));
        }
        if setup.surround.mode == outfx::Mode::Off {
            return say(message, format_args!("surround is off: choose sq, qs or ambience to decode"));
        }
        if self.run.is_some() {
            return;
        }
        let Some(raw) = self.latest_raw() else {
            return say(message, format_args!("no {}.lr.flac take to decode", self.name.as_str()));
        };
        let Some(out) = Names::decoded_for(raw.as_bytes()) else { return };
        let api = self.api;
        let Some(length) = file_length(api, raw.as_str()) else {
            return say(message, format_args!("cannot find {}", raw.as_str()));
        };
        let size = length as usize;
        let buffer = (api.alloc)(size);
        if buffer == 0 {
            return say(message, format_args!("not enough memory for {} ({} MB)", raw.as_str(), size >> 20));
        }
        let read = (api.fs_read)(raw.as_bytes().as_ptr(), raw.as_bytes().len(), buffer as *mut u8, size);
        if read < 0 || read as usize != size {
            (api.free)(buffer, size);
            return say(message, format_args!("cannot read {}", raw.as_str()));
        }
        let bytes: &'static [u8] = unsafe { core::slice::from_raw_parts(buffer as *const u8, size) };
        let (decoder, surround, transcoder) = unsafe { (&mut *core::ptr::addr_of_mut!(DECODER), &mut *core::ptr::addr_of_mut!(PASS_SURROUND), &mut *core::ptr::addr_of_mut!(TRANSCODER)) };
        *decoder = Decoder::new(Bytes(bytes));
        if decoder.open().is_err() {
            (api.free)(buffer, size);
            return say(message, format_args!("{} is not a FLAC file", raw.as_str()));
        }
        surround.tune(decoder.sample_rate, setup.surround);
        let mut eq = Phono::new(decoder.sample_rate);
        setup.apply(&mut eq);
        unsafe { *core::ptr::addr_of_mut!(PASS_EQ) = Some(eq) };
        let Some(eq) = (unsafe { (*core::ptr::addr_of_mut!(PASS_EQ)).as_mut() }) else { return };
        if let Err(error) = transcoder.start(decoder, surround, eq, setup.depth) {
            (api.free)(buffer, size);
            return say(message, format_args!("{}", decode_error(error)));
        }
        // The decoded file is derived from the raw take, which stays: an older one is replaced.
        if exists(api, out.as_bytes()) {
            (api.fs_remove)(out.as_bytes().as_ptr(), out.as_bytes().len());
        }
        self.percent = 0;
        self.run = Some(DecodeRun { buffer, size, raw, out, settings: setup.surround, curve: setup.curve });
        say(message, format_args!("decoding {} to {}", raw.as_str(), out.as_str()));
    }

    pub fn cancel_decode(&mut self, message: &mut Line<64>) {
        if let Some(run) = self.run {
            unsafe { (*core::ptr::addr_of_mut!(TRANSCODER)).cancel() };
            file().abandon(sink());
            self.end_decode();
            say(message, format_args!("decode cancelled; {} is incomplete", run.out.as_str()));
        }
    }

    fn end_decode(&mut self) {
        if let Some(run) = self.run.take() {
            (self.api.free)(run.buffer, run.size);
        }
    }

    /// Leaves cleanly: a running take is finished (so its file is complete), a decode is dropped.
    pub fn shut_down(&mut self, output: &mut Output, message: &mut Line<64>) {
        self.stop_recording(output, message);
        self.cancel_decode(message);
    }

    pub fn begin_edit(&mut self) {
        let mut text = Line::new();
        set(&mut text, self.name.as_str());
        self.editing = Some(text);
    }

    /// A key while the name is edited. Enter keeps it if it is a valid name; Escape (the caller) drops the edit.
    pub fn edit_key(&mut self, key: Key, message: &mut Line<64>) {
        let Some(text) = self.editing.as_mut() else { return };
        match key {
            Key::Enter => match Names::parse(text.as_str()) {
                Ok(names) => {
                    set(&mut self.name, text.as_str());
                    self.editing = None;
                    say(message, format_args!("name {}: files {} and {}", names.base(), names.raw(1).as_str(), names.decoded(1).as_str()));
                }
                Err(error) => name_error(error, message),
            },
            Key::Backspace => text.length = text.length.saturating_sub(1),
            Key::Char(byte) => {
                if let Some(byte) = names::typed(byte) {
                    if text.length < text.bytes.len() {
                        text.bytes[text.length] = byte;
                        text.length += 1;
                    }
                }
            }
            _ => {}
        }
    }

    /// `REC 12:34 45 MB` or `decoding 42%`, for the header; empty when idle.
    pub fn status(&self, out: &mut Line<48>) {
        out.clear();
        if self.recording {
            let seconds = file().samples() / self.rate.max(1) as u64;
            let _ = write!(out, "REC {}:{:02} {} MB", seconds / 60, seconds % 60, file().bytes() >> 20);
        } else if self.run.is_some() {
            let _ = write!(out, "decoding {}%", self.percent);
        }
    }
}

fn file_error(error: FileError) -> &'static str {
    match error {
        FileError::Full => "the disk is full",
        FileError::Sink(_) => "the disk refused the write",
        FileError::Format => "bad format",
        FileError::NotOpen => "not open",
    }
}

fn decode_error(error: DecodeError) -> &'static str {
    match error {
        DecodeError::Decode(_) => "the take does not decode",
        DecodeError::File(error) => file_error(error),
        DecodeError::NotStereo => "the take is not stereo",
        DecodeError::NoDecoder => "surround is off",
    }
}
