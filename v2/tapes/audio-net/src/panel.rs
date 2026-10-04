//! The settings panel: `tape run audio-net` with no arguments. A full-screen form for the things a user decides
//! (mode, network, peer, codec, channels), a peer picker fed by the announcements, and a live view of the running
//! stream with level meters. It draws with the few ANSI sequences the console and every terminal know (clear, cursor
//! position, erase to end of line) and reads keys through `read_key`, so it works on the local screen and over `term
//! net` alike. Settings are loaded from and saved to `audio.cfg` (the same file `audio-net @audio.cfg` runs), and the
//! panel can write the `start` file that launches it at boot.

use crate::abort;
use crate::igmp::Igmp;
use crate::net::{self, DhcpStep, DhcpTask};
use crate::stats;
use audio_wire::sap;
use tape_net::MacAddress;
use crate::{MacText, Options, Transport, parse, peers, run_once};
use audio_wire::{ChannelSet, Cursor, meter};
use core::fmt::Write;
use tape_rt::{AudioInfo, Key, KeyDecoder, TapeApi};

const CONFIG_FILE: &str = "audio.cfg";
const START_FILE: &str = "start";
const START_LINE: &str = "run audio-net @audio.cfg";

struct Screen;

impl Write for Screen {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            (tape_rt::api().putc)(u32::from(byte));
        }
        Ok(())
    }
}

macro_rules! draw {
    ($($argument:tt)*) => {{ let _ = Screen.write_fmt(format_args!($($argument)*)); }};
}

fn at(row: usize, column: usize) {
    draw!("\x1b[{row};{column}H");
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Text<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Text<N> {
    const fn new() -> Self {
        Text { bytes: [0; N], length: 0 }
    }
    fn from(text: &str) -> Self {
        let mut result = Self::new();
        for &byte in text.as_bytes() {
            result.push(byte);
        }
        result
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
    fn push(&mut self, byte: u8) {
        if self.length < N {
            self.bytes[self.length] = byte;
            self.length += 1;
        }
    }
    fn pop(&mut self) {
        self.length = self.length.saturating_sub(1);
    }
    fn is_empty(&self) -> bool {
        self.length == 0
    }
}

impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            self.push(byte);
        }
        Ok(())
    }
}

const MODES: [&str; 3] = ["Send", "Receive", "Duplex (send and receive)"];
const NETWORKS: [&str; 3] = ["RTP over IP (routable, AES67-style)", "Raw Ethernet (same switch, no IP)", "AVTP (AVB, same switch, no IP)"];
const CODECS: [&str; 5] = ["PCM 24-bit (best quality)", "PCM 16-bit", "Opus, balanced (96 kbit/s per stereo pair)", "Opus, low bandwidth (48 kbit/s per pair)", "Opus, low delay (128 kbit/s per pair, 5 ms)"];

const R_MODE: usize = 0;
const R_NETWORK: usize = 1;
const R_NIC: usize = 2;
const R_ADDRESSING: usize = 3;
const R_IP: usize = 4;
const R_GATEWAY: usize = 5;
const R_PEER: usize = 6;
const R_CODEC: usize = 7;
const R_SEND: usize = 8;
const R_RECEIVE: usize = 9;
const R_NAME: usize = 10;
const R_GROUP: usize = 11;
const ROWS: usize = 12;
const LABELS: [&str; ROWS] = ["Mode", "Network", "NIC", "Addressing", "IP address", "Gateway", "Peer", "Codec", "Send channels", "Receive channels", "Name", "Multicast group"];

#[derive(Clone, Copy)]
struct Settings {
    mode: usize,
    network: usize,
    nic: usize,
    codec: usize,
    /// Get the address from a DHCP server (the usual case) instead of the static `ip` and `gateway`.
    dhcp: bool,
    ip: Text<24>,
    gateway: Text<16>,
    peer: Text<24>,
    name: Text<24>,
    group: Text<16>,
    send: Text<24>,
    receive: Text<24>,
}

impl Settings {
    fn new() -> Settings {
        Settings { mode: 0, network: 0, nic: 1, codec: 0, dhcp: true, ip: Text::from("192.168.1.20/24"), gateway: Text::new(), peer: Text::new(), name: Text::new(), group: Text::new(), send: Text::from("1-2"), receive: Text::from("1-2") }
    }

    fn rtp(&self) -> bool {
        self.network == 0
    }

    fn applies(&self, row: usize) -> bool {
        match row {
            R_ADDRESSING | R_NAME | R_GROUP => self.rtp(),
            R_IP | R_GATEWAY => self.rtp() && !self.dhcp,
            R_SEND => self.mode != 1,
            R_RECEIVE => self.mode != 0,
            _ => true,
        }
    }

    fn text_mut(&mut self, row: usize) -> Option<&mut dyn TextField> {
        match row {
            R_IP => Some(&mut self.ip),
            R_GATEWAY => Some(&mut self.gateway),
            R_PEER => Some(&mut self.peer),
            R_NAME => Some(&mut self.name),
            R_GROUP => Some(&mut self.group),
            R_SEND => Some(&mut self.send),
            R_RECEIVE => Some(&mut self.receive),
            _ => None,
        }
    }

    /// The settings as a configuration file (`audio-net @audio.cfg` runs it).
    fn cfg(&self, out: &mut Cursor<'_>, for_run: bool) {
        let _ = writeln!(out, "# audio-net configuration, written by the panel");
        let _ = writeln!(out, "mode {}", ["send", "recv", "duplex"][self.mode]);
        let _ = writeln!(out, "proto {}", ["rtp", "raw", "avtp"][self.network]);
        let _ = writeln!(out, "nic {}", self.nic);
        if self.rtp() {
            if self.dhcp {
                let _ = writeln!(out, "ip dhcp");
            }
            for (key, value) in [("ip", if self.dhcp { "" } else { self.ip.as_str() }), ("gw", if self.dhcp { "" } else { self.gateway.as_str() }), ("name", self.name.as_str()), ("group", self.group.as_str())] {
                if !value.is_empty() {
                    let _ = writeln!(out, "{key} {value}");
                }
            }
        }
        let peer = self.peer.as_str();
        if !peer.is_empty() {
            if self.mode != 1 {
                let _ = writeln!(out, "to {peer}");
            }
            if self.mode != 0 {
                let _ = writeln!(out, "{} {peer}", if self.rtp() { "source" } else { "from" });
            }
        }
        match self.codec {
            0 => {
                let _ = writeln!(out, "bits 24");
            }
            1 => {
                let _ = writeln!(out, "bits 16");
            }
            2 => {
                let _ = writeln!(out, "codec opus\nbitrate 96\nframe 20");
            }
            3 => {
                let _ = writeln!(out, "codec opus\nbitrate 48\nframe 20");
            }
            _ => {
                let _ = writeln!(out, "codec opus\nbitrate 128\nframe 5");
            }
        }
        if for_run && self.mode != 1 && read_audio_info().capture_channels == 0 {
            let _ = writeln!(out, "src tone=440"); // no device with inputs: send a test tone
        }
        match self.mode {
            0 => {
                let _ = writeln!(out, "ch {}", self.send.as_str());
            }
            1 => {
                let _ = writeln!(out, "ch {}", self.receive.as_str());
            }
            _ => {
                let _ = writeln!(out, "in {}\nout {}", self.send.as_str(), self.receive.as_str());
            }
        }
        if self.mode != 0 {
            let count = ChannelSet::parse(self.receive.as_str()).map_or(2, |set| set.count());
            let _ = writeln!(out, "chan {count}");
        }
    }

    /// Reads a configuration file's words back into the settings (what the panel can express; the rest is ignored).
    fn load(&mut self, words: &[&str]) {
        let mode_of = |word: &str| match word {
            "send" => Some(0),
            "recv" => Some(1),
            "duplex" => Some(2),
            _ => None,
        };
        for word in words {
            if let Some(mode) = mode_of(word.strip_prefix("mode=").unwrap_or(word)) {
                self.mode = mode;
            }
        }
        let (mut opus, mut bitrate, mut frame, mut bits) = (false, 96, 20, 24);
        for word in words {
            let Some((key, value)) = word.split_once('=') else { continue };
            match key {
                "proto" => self.network = ["rtp", "raw", "avtp"].iter().position(|name| *name == value).unwrap_or(0),
                "nic" => self.nic = value.parse().unwrap_or(1),
                "ip" if value == "dhcp" => self.dhcp = true,
                "ip" => (self.dhcp, self.ip) = (false, Text::from(value)),
                "gw" => self.gateway = Text::from(value),
                "name" => self.name = Text::from(value),
                "group" => self.group = Text::from(value),
                "to" | "source" | "from" => self.peer = Text::from(value),
                "codec" => opus = value == "opus",
                "bitrate" => bitrate = value.parse().unwrap_or(96),
                "frame" => frame = value.parse().unwrap_or(20),
                "bits" => bits = value.parse().unwrap_or(24),
                "ch" if self.mode == 1 => self.receive = Text::from(value),
                "ch" | "in" => self.send = Text::from(value),
                "out" => self.receive = Text::from(value),
                _ => {}
            }
        }
        self.codec = if opus {
            if frame < 20 { 4 } else if bitrate <= 64 { 3 } else { 2 }
        } else if bits == 16 {
            1
        } else {
            0
        };
        if !self.rtp() && self.codec >= 2 {
            self.codec = 0;
        }
    }
}

trait TextField {
    fn push(&mut self, byte: u8);
    fn pop(&mut self);
    fn clear(&mut self);
    fn current(&self) -> &str;
}

impl<const N: usize> TextField for Text<N> {
    fn push(&mut self, byte: u8) {
        Text::push(self, byte)
    }
    fn pop(&mut self) {
        Text::pop(self)
    }
    fn clear(&mut self) {
        self.length = 0;
    }
    fn current(&self) -> &str {
        self.as_str()
    }
}

static mut CFG_BUFFER: [u8; 1024] = [0; 1024];

/// The settings as configuration text, in a static buffer (the words parsed from it borrow it).
fn cfg_text(settings: &Settings, for_run: bool) -> &'static str {
    let buffer: &'static mut [u8; 1024] = unsafe { &mut *(&raw mut CFG_BUFFER) };
    let mut cursor = Cursor { bytes: &mut buffer[..], length: 0 };
    settings.cfg(&mut cursor, for_run);
    let length = cursor.length;
    core::str::from_utf8(&buffer[..length]).unwrap_or("")
}

/// What the settings would run: the mode and the options, or why they are not a valid stream.
fn build(settings: &Settings) -> Result<(&'static str, Options<'static>), &'static str> {
    let text = cfg_text(settings, true);
    let mut words: [&'static str; crate::MAX_WORDS] = [""; crate::MAX_WORDS];
    let mut count = 0;
    crate::normalize(text, &mut words, &mut count)?;
    let mut mode = "";
    let mut kept: [&'static str; crate::MAX_WORDS] = [""; crate::MAX_WORDS];
    let mut kept_count = 0;
    for &word in &words[..count] {
        match word.strip_prefix("mode=").unwrap_or(word) {
            "send" | "recv" | "duplex" => mode = word.strip_prefix("mode=").unwrap_or(word),
            _ => {
                kept[kept_count] = word;
                kept_count += 1;
            }
        }
    }
    let options = parse(kept[..kept_count].iter().copied())?;
    if (tape_rt::api().nic_count)() <= options.nic {
        return Err("this machine has no such NIC");
    }
    Ok((mode, options))
}

fn value_text(settings: &Settings, row: usize, info: &AudioInfo, out: &mut Text<100>) {
    let dash = |text: &str, out: &mut Text<100>| {
        let _ = write!(out, "{}", if text.is_empty() { "-" } else { text });
    };
    if !settings.applies(row) {
        let _ = write!(out, "{}", if settings.rtp() && settings.dhcp && matches!(row, R_IP | R_GATEWAY) { "(from DHCP)" } else { "-" });
        return;
    }
    match row {
        R_MODE => {
            let _ = write!(out, "{}", MODES[settings.mode]);
        }
        R_NETWORK => {
            let _ = write!(out, "{}", NETWORKS[settings.network]);
        }
        R_NIC => {
            let mut mac = [0u8; 6];
            (tape_rt::api().nic_mac)(settings.nic - 1, mac.as_mut_ptr());
            let _ = write!(out, "{}   MAC {}", settings.nic, MacText(mac));
        }
        R_CODEC => {
            let _ = write!(out, "{}", CODECS[settings.codec]);
        }
        R_ADDRESSING => {
            let _ = write!(out, "{}", if settings.dhcp { "DHCP (the address comes from the network's DHCP server)" } else { "Static (type the address below)" });
        }
        R_PEER => {
            if settings.peer.is_empty() {
                let _ = write!(out, "{}", if settings.mode == 1 { "-  (any sender; P picks from the announced ones)" } else { "-  (P picks from the announced ones)" });
            } else {
                let _ = write!(out, "{}", settings.peer.as_str());
            }
        }
        R_SEND | R_RECEIVE => {
            let text = if row == R_SEND { settings.send.as_str() } else { settings.receive.as_str() };
            let available = if row == R_SEND { info.capture_channels } else { info.playback_channels };
            if available == 0 {
                let _ = write!(out, "{text}   (no audio device{})", if row == R_SEND { ": a test tone is sent" } else { ": nothing can be played" });
            } else {
                let _ = write!(out, "{text}   (the device has {available})");
            }
        }
        R_IP => dash(settings.ip.as_str(), out),
        R_GATEWAY => dash(settings.gateway.as_str(), out),
        R_NAME => {
            if settings.name.is_empty() {
                let _ = write!(out, "{}  (automatic: fos- and the end of the MAC; type a name to change it)", effective_name(settings).as_str());
            } else {
                let _ = write!(out, "{}", settings.name.as_str());
            }
        }
        _ => dash(settings.group.as_str(), out),
    }
}

/// An estimate of the bandwidth on the wire, in kbit/s.
fn bandwidth(settings: &Settings) -> u32 {
    let channels = |text: &str| ChannelSet::parse(text).map_or(2, |set| set.count() as u32);
    let (send, receive) = (channels(settings.send.as_str()), channels(settings.receive.as_str()));
    let per = |count: u32| match settings.codec {
        0 => count * 24 * 48 * 112 / 100,
        1 => count * 16 * 48 * 112 / 100,
        2 => count.div_ceil(2) * 96,
        3 => count.div_ceil(2) * 48,
        _ => count.div_ceil(2) * 128,
    };
    match settings.mode {
        0 => per(send),
        1 => per(receive),
        _ => per(send).max(per(receive)),
    }
}

fn read_audio_info() -> AudioInfo {
    let mut info = AudioInfo::default();
    if (tape_rt::api().audio_info)(&mut info) == 0 {
        info = AudioInfo::default();
    }
    info
}

fn draw_form(settings: &Settings, selected: usize, editing: bool, info: &AudioInfo, message: &str) {
    draw!("\x1b[2J");
    at(1, 1);
    draw!(" fos audio-net: settings        this machine is called {}\x1b[K", effective_name(settings).as_str());
    at(2, 1);
    if info.capture_channels == 0 && info.playback_channels == 0 {
        draw!(" Device: no USB audio device found (send can use a test tone)\x1b[K");
    } else {
        draw!(" Device: USB audio (class {}.0), {} inputs / {} outputs, {} bit\x1b[K", info.class_version, info.capture_channels, info.playback_channels, info.capture_bits);
    }
    at(3, 1);
    draw!(" {:-<78}", "");
    for row in 0..ROWS {
        draw_row(settings, row, selected, editing, info);
    }
    draw_info(settings, message);
    at(5 + ROWS + 4, 1);
    draw!(" Up/Down or Tab: select   Left/Right or Enter: change   S: start   P: pick a peer\x1b[K");
    at(5 + ROWS + 5, 1);
    draw!(" W: save {CONFIG_FILE}   B: start at boot   Q or Esc: quit\x1b[K");
}

/// The bandwidth estimate and the message line.
fn draw_info(settings: &Settings, message: &str) {
    at(5 + ROWS + 1, 1);
    let kbit = bandwidth(settings);
    if kbit >= 1000 {
        draw!(" Needs about {}.{} Mbit/s on the network.\x1b[K", kbit / 1000, kbit % 1000 / 100);
    } else {
        draw!(" Needs about {kbit} kbit/s on the network.\x1b[K");
    }
    draw_message(message);
}

fn draw_row(settings: &Settings, row: usize, selected: usize, editing: bool, info: &AudioInfo) {
    at(5 + row, 1);
    let mut value = Text::<100>::new();
    value_text(settings, row, info, &mut value);
    let marker = if row == selected { '>' } else { ' ' };
    if editing && row == selected {
        let current = match row {
            R_IP => settings.ip.as_str(),
            R_GATEWAY => settings.gateway.as_str(),
            R_PEER => settings.peer.as_str(),
            R_NAME => settings.name.as_str(),
            R_GROUP => settings.group.as_str(),
            R_SEND => settings.send.as_str(),
            _ => settings.receive.as_str(),
        };
        draw!(" {marker} {:<18}{current}_   (Enter: done, Esc: cancel)\x1b[K", LABELS[row]);
    } else {
        draw!(" {marker} {:<18}{}\x1b[K", LABELS[row], value.as_str());
    }
}

fn draw_message(message: &str) {
    at(5 + ROWS + 2, 1);
    draw!(" {message}\x1b[K");
}

/// A key from `read_key`, decoded: None if nothing (or half an escape sequence) is there yet.
fn next_key(decoder: &mut KeyDecoder) -> Option<Key> {
    let event = (tape_rt::api().read_key)();
    match event {
        0 => None,
        1 => Some(Key::Control(0x1b)),
        other => decoder.feed((other & 0xff) as u8),
    }
}

fn is_escape(key: &Key) -> bool {
    matches!(key, Key::Control(0x1b))
}

/// Cycles an enumerated row by `step` (+1 or -1) or adjusts the NIC.
fn cycle(settings: &mut Settings, row: usize, step: isize) {
    let wrap = |value: usize, count: usize| ((value as isize + step).rem_euclid(count as isize)) as usize;
    match row {
        R_MODE => settings.mode = wrap(settings.mode, 3),
        R_ADDRESSING => settings.dhcp = !settings.dhcp,
        R_NETWORK => {
            settings.network = wrap(settings.network, 3);
            if !settings.rtp() && settings.codec >= 2 {
                settings.codec = 0; // Opus needs RTP
            }
        }
        R_NIC => settings.nic = 1 + wrap(settings.nic - 1, ((tape_rt::api().nic_count)()).max(1)),
        R_CODEC => {
            let count = if settings.rtp() { CODECS.len() } else { 2 };
            settings.codec = wrap(settings.codec, count);
        }
        _ => {}
    }
}

fn load_settings(settings: &mut Settings) {
    let mut words: [&'static str; crate::MAX_WORDS] = [""; crate::MAX_WORDS];
    let mut count = 0;
    let api = tape_rt::api();
    let mut buffer = [0u8; 1024];
    let length = (api.fs_read)(CONFIG_FILE.as_ptr(), CONFIG_FILE.len(), buffer.as_mut_ptr(), buffer.len());
    if length > 0 {
        if let Ok(text) = core::str::from_utf8(&buffer[..length as usize]) {
            if crate::normalize(text, &mut words, &mut count).is_ok() {
                settings.load(&words[..count]);
            }
        }
    }
}

fn save_settings(settings: &Settings) -> &'static str {
    let text = cfg_text(settings, false);
    let api = tape_rt::api();
    if (api.fs_write)(CONFIG_FILE.as_ptr(), CONFIG_FILE.len(), text.as_ptr(), text.len()) == 0 {
        "Saved audio.cfg. `audio-net @audio.cfg` starts exactly this."
    } else {
        "Could not write audio.cfg (is the volume full?)."
    }
}

/// B: make the machine start streaming at boot, with a `start` file running the saved configuration.
fn toggle_boot(settings: &Settings) -> &'static str {
    let api = tape_rt::api();
    let mut buffer = [0u8; 512];
    let length = (api.fs_read)(START_FILE.as_ptr(), START_FILE.len(), buffer.as_mut_ptr(), buffer.len());
    let existing = if length > 0 { core::str::from_utf8(&buffer[..length as usize]).unwrap_or("?") } else { "" };
    if existing.trim().is_empty() {
        let saved = save_settings(settings);
        if (api.fs_write)(START_FILE.as_ptr(), START_FILE.len(), START_LINE.as_ptr(), START_LINE.len()) == 0 {
            if saved.starts_with("Saved") { "Saved audio.cfg and a start file: this machine now streams at boot." } else { saved }
        } else {
            "Could not write the start file."
        }
    } else if existing.trim() == START_LINE {
        let _ = (api.fs_remove)(START_FILE.as_ptr(), START_FILE.len());
        "Removed the start file: this machine no longer streams at boot."
    } else {
        "A start file with other commands exists; add the line `run audio-net @audio.cfg` to it yourself."
    }
}

/// Where the meters are drawn on the live screen (a 24-row terminal must fit them).
const BAR_COL: usize = 9;
/// Cells 0..=60, one per dB from -60 dBFS to full scale.
const CELLS: i32 = 61;
const NUMBER_COL: usize = 71;
const CLIP_COL: usize = 77;
const ROW_SCALE: usize = 7;
const ROW_FIRST: usize = 9;
const PAGE_ROWS: usize = 12;
/// The Nordic scale's TEST (0) is -18 dBFS: cell 42. The permitted maximum level (+9) is -9 dBFS: cell 51.
const TEST_CELL: i32 = 42;
const PML_CELL: i32 = 51;
const HOLD_MICROS: u64 = 3_000_000;
const MAX_ROWS: usize = 2 * stats::MAX_CHANNELS;

/// The live view's state, shared with the hook that draws it from inside the stream loops.
struct Run {
    stop: bool,
    next_draw: u64,
    started: u64,
    /// One row per channel sent, then one per channel received; each says which device channel it is.
    rows: usize,
    sends: usize,
    labels: [u8; MAX_ROWS],
    page: usize,
    /// The scale and the numbers in Nordic units (TEST = 0) instead of dBFS.
    nordic: bool,
    /// Rows (and the footer) still to be drawn from scratch; the drawing is spread over many calls so that none of
    /// them holds the stream up for long.
    needs_full: [bool; MAX_ROWS],
    footer_dirty: bool,
    cursor: usize,
    next_counters: u64,
    ip_shown: bool,
    mac: [u8; 6],
    nic: usize,
    counters: Text<100>,
    message: Text<100>,
    actual: Text<100>,
    shown_cell: [i16; MAX_ROWS],
    shown_hold: [i16; MAX_ROWS],
    shown_number: [i16; MAX_ROWS],
    shown_clip: [bool; MAX_ROWS],
    hold: [u32; MAX_ROWS],
    hold_at: [u64; MAX_ROWS],
    clip_at: [u64; MAX_ROWS],
    /// The longest the panel's own drawing has held up the stream loop since the run began (microseconds).
    ui_worst: u64,
}

const fn blank_run() -> Run {
    Run {
        stop: false, next_draw: 0, started: 0, rows: 0, sends: 0, labels: [0; MAX_ROWS], page: 0, nordic: true, needs_full: [true; MAX_ROWS], footer_dirty: true, cursor: 0, next_counters: 0, ip_shown: false, mac: [0; 6], nic: 0,
        counters: Text::new(), message: Text::new(), actual: Text::new(), shown_cell: [-1; MAX_ROWS], shown_hold: [-1; MAX_ROWS], shown_number: [i16::MIN; MAX_ROWS], shown_clip: [false; MAX_ROWS],
        hold: [0; MAX_ROWS], hold_at: [0; MAX_ROWS], clip_at: [0; MAX_ROWS], ui_worst: 0,
    }
}

impl Run {
    /// Everything on the meter screen is redrawn from scratch over the next calls (new page, new scale).
    fn mark_all(&mut self) {
        self.needs_full = [true; MAX_ROWS];
        self.footer_dirty = true;
        self.cursor = 0;
    }
}

static mut RUN: Run = blank_run();

fn run_state() -> &'static mut Run {
    unsafe { &mut *(&raw mut RUN) }
}

/// The character in cell `cell` of a bar whose level is at `level` (and peak-hold marker at `hold`): filled up to the
/// level (`=`, and `#` above the permitted maximum), the hold marker `|`, else the scale: `:` at TEST, `.` every 6 dB.
fn bar_char(cell: i32, level: i32, hold: i32) -> char {
    if cell <= level {
        if cell > PML_CELL { '#' } else { '=' }
    } else if cell == hold {
        '|'
    } else if cell == TEST_CELL {
        ':'
    } else if cell % 6 == 0 {
        '.'
    } else {
        ' '
    }
}

/// A level in tenths of a dB as text of six columns, signed with one decimal.
fn tenths_text(tenths: i32, out: &mut Text<8>) {
    let magnitude = tenths.unsigned_abs();
    let _ = write!(out, "{:>6}", {
        let mut text = Text::<8>::new();
        let _ = write!(text, "{}{}.{}", if tenths < 0 { '-' } else { '+' }, magnitude / 10, magnitude % 10);
        text
    }.as_str());
}

fn draw_scale(run: &Run) {
    at(ROW_SCALE, 1);
    draw!("\x1b[K");
    for mark in (-42..=18).step_by(6) {
        let cell = (mark + 42) as usize; // the cell is the same on both scales (the Nordic number is dBFS + 18)
        let value = if run.nordic { mark } else { mark - 18 };
        let mut label = Text::<8>::new();
        let _ = write!(label, "{value}");
        let column = (BAR_COL + cell).saturating_sub((label.length.max(1) - 1) / 2).max(1);
        at(ROW_SCALE, column);
        draw!("{}", label.as_str());
    }
    at(ROW_SCALE + 1, 1);
    draw!("\x1b[K");
    at(ROW_SCALE + 1, 2);
    draw!("{}", if run.nordic { "Nordic" } else { "dBFS" });
    for mark in (0..CELLS).step_by(6) {
        at(ROW_SCALE + 1, BAR_COL + mark as usize);
        draw!("{}", if mark == TEST_CELL { 'T' } else { '|' });
    }
    at(ROW_SCALE + 1, BAR_COL + PML_CELL as usize);
    draw!("M");
}

fn draw_footer(run: &Run) {
    let first = run.page * PAGE_ROWS;
    let last = (first + PAGE_ROWS).min(run.rows);
    at(ROW_FIRST + PAGE_ROWS, 2);
    draw!("Channels {}-{} of {}   N: next page   B: previous page\x1b[K", first + 1, last, run.rows);
    at(ROW_FIRST + PAGE_ROWS + 1, 2);
    draw!("T = TEST level, -18 dBFS    M = maximum permitted level, -9 dBFS    # = above it\x1b[K");
    at(ROW_FIRST + PAGE_ROWS + 2, 2);
    draw!("U: Nordic/dBFS   R: reset hold   M: meters off/on   Esc or Q: stop and return\x1b[K");
}

/// Rows drawn per call: each call is short, and a whole page is refreshed within 100 ms.
const ROWS_PER_CALL: usize = 3;

/// Brings the visible meter rows up to date, a few rows per call. Reads each channel's detector (the highest
/// reading since the last look, so a short peak between looks still shows), keeps a three-second peak hold and a
/// latched clip indicator, and draws only what changed.
fn update_meters(run: &mut Run, now: u64) {
    let shown = stats::get();
    let first = run.page * PAGE_ROWS;
    for _ in 0..ROWS_PER_CALL {
        let slot = run.cursor;
        run.cursor = (run.cursor + 1) % PAGE_ROWS;
        let index = first + slot;
        let row = ROW_FIRST + slot;
        if index >= run.rows {
            let flag = index.min(MAX_ROWS - 1);
            if run.needs_full[flag] {
                at(row, 1);
                draw!("\x1b[K");
                run.needs_full[flag] = false;
            }
            continue;
        }
        let full = run.needs_full[index];
        run.needs_full[index] = false;
        let is_send = index < run.sends;
        let channel = if is_send { index } else { index - run.sends };
        let (detector, clip_flag) = if is_send { (&mut shown.send[channel], &mut shown.send_clip[channel]) } else { (&mut shown.receive[channel], &mut shown.receive_clip[channel]) };
        let peak = detector.take_max();
        if *clip_flag {
            *clip_flag = false;
            run.clip_at[index] = now;
        }
        if peak >= run.hold[index] || now > run.hold_at[index] + HOLD_MICROS {
            run.hold[index] = peak;
            run.hold_at[index] = now;
        }
        let tenths = meter::db_tenths(peak);
        let cell = ((tenths + 600).div_euclid(10)).clamp(0, CELLS - 1);
        let hold_cell = ((meter::db_tenths(run.hold[index]) + 600).div_euclid(10)).clamp(0, CELLS - 1);
        let hold_cell = if hold_cell > cell && run.hold[index] >= 64 { hold_cell } else { -1 };
        let number = if peak < 64 { i16::MAX } else { (tenths + if run.nordic { 180 } else { 0 }) as i16 };
        let clipped = now < run.clip_at[index] + HOLD_MICROS && run.clip_at[index] != 0;
        if full {
            at(row, 1);
            draw!("\x1b[K");
            at(row, 2);
            draw!("{}{:>2}", if is_send { "In " } else { "Out" }, run.labels[index]);
        }
        let (old_cell, old_hold) = (i32::from(run.shown_cell[index]), i32::from(run.shown_hold[index]));
        if full || old_cell != cell || old_hold != hold_cell {
            let (low, high) = if full {
                (0, CELLS - 1)
            } else {
                let mut low = cell.min(old_cell.max(0));
                let mut high = cell.max(old_cell);
                for marker in [hold_cell, old_hold] {
                    if marker >= 0 {
                        low = low.min(marker);
                        high = high.max(marker);
                    }
                }
                (low.max(0), high.min(CELLS - 1))
            };
            at(row, BAR_COL + low as usize);
            for c in low..=high {
                draw!("{}", bar_char(c, cell, hold_cell));
            }
            run.shown_cell[index] = cell as i16;
            run.shown_hold[index] = hold_cell as i16;
        }
        if full || run.shown_number[index] != number {
            at(row, NUMBER_COL);
            if number == i16::MAX {
                draw!("  -inf");
            } else {
                let mut text = Text::<8>::new();
                tenths_text(i32::from(number), &mut text);
                draw!("{}", text.as_str());
            }
            run.shown_number[index] = number;
        }
        if full || run.shown_clip[index] != clipped {
            at(row, CLIP_COL);
            draw!("{}", if clipped { "CLIP" } else { "    " });
            run.shown_clip[index] = clipped;
        }
    }
}

/// Called from inside the stream loops (as `should_abort`): reads a key, and ten times a second brings the counters,
/// the last message and the meters up to date. Only what changed is sent, because every byte costs time the stream
/// needs.
extern "C" fn panel_hook() -> u32 {
    let run = run_state();
    if run.stop {
        return 1;
    }
    let event = (tape_rt::api().read_key)();
    if event == 1 {
        run.stop = true;
        return 1;
    }
    if event >= 0x100 {
        match (event & 0xff) as u8 {
            b'q' | b'Q' => {
                run.stop = true;
                return 1;
            }
            b'n' | b'N' => {
                run.page = (run.page + 1) % run.rows.div_ceil(PAGE_ROWS).max(1);
                run.mark_all();
            }
            b'b' | b'B' => {
                let pages = run.rows.div_ceil(PAGE_ROWS).max(1);
                run.page = (run.page + pages - 1) % pages;
                run.mark_all();
            }
            b'u' | b'U' => {
                run.nordic = !run.nordic;
                run.mark_all();
                draw_scale(run);
            }
            b'r' | b'R' => {
                run.hold = [0; MAX_ROWS];
                run.clip_at = [0; MAX_ROWS];
            }
            b'm' | b'M' => {
                // Meters off: no detector work per sample and no meter drawing, in case the machine is too slow for
                // them (the counters line shows the longest the drawing has held the stream up).
                let stats = stats::get();
                stats.meters_on = !stats.meters_on;
                for slot in 0..PAGE_ROWS {
                    at(ROW_FIRST + slot, 1);
                    draw!("\x1b[K");
                }
                if stats.meters_on {
                    run.mark_all();
                } else {
                    at(ROW_FIRST, 2);
                    draw!("Meters are off (M turns them on again).");
                }
            }
            _ => {}
        }
    }
    let now = tape_rt::micros();
    if now < run.next_draw {
        return 0;
    }
    run.next_draw = now + 25_000;
    // One small piece of drawing per call, so that no call holds the stream up for long.
    if run.footer_dirty && stats::get().meters_on {
        run.footer_dirty = false;
        draw_footer(run);
    } else if now >= run.next_counters {
        run.next_counters = now + 250_000;
        if !run.ip_shown {
            if let Some(ip) = crate::current_ip() {
                run.ip_shown = true;
                at(3, 1);
                draw!(" NIC {}  MAC {}  IP {}.{}.{}.{}\x1b[K", run.nic + 1, MacText(run.mac), ip[0], ip[1], ip[2], ip[3]);
            }
        }
        let shown = stats::get();
        let mut counters = Text::<100>::new();
        let seconds = (now - run.started) / 1_000_000;
        let _ = write!(counters, "Sent {} Recv {} Lost {} Ring {}/{} ms Clock {:+} ppm UI stall {} ms {}:{:02}", shown.sent, shown.received, shown.lost, ms(shown.level_frames, shown.rate), ms(shown.target_frames, shown.rate), shown.offset_ppm, run.ui_worst / 1000, seconds / 60, seconds % 60);
        if counters != run.counters {
            at(5, 2);
            draw!("{}\x1b[K", counters.as_str());
            run.counters = counters;
        }
        // What is really sent and received: a peer's announcement can replace the choice made on this machine.
        let mut actual = Text::<100>::new();
        let _ = write!(actual, "Sending: {}   Receiving: {}", if shown.send_codec.as_str().is_empty() { "-" } else { shown.send_codec.as_str() }, if shown.receive_codec.as_str().is_empty() { "-" } else { shown.receive_codec.as_str() });
        if actual != run.actual {
            at(4, 2);
            draw!("{}\x1b[K", actual.as_str());
            run.actual = actual;
        }
        let message = Text::<100>::from(tape_rt::last_message());
        if message != run.message {
            at(6, 2);
            draw!("{}\x1b[K", message.as_str());
            run.message = message;
        }
    } else if stats::get().meters_on {
        update_meters(run, now);
    }
    // How long this call took: the stream loop was not running meanwhile.
    run.ui_worst = run.ui_worst.max(tape_rt::micros() - now);
    0
}

fn ms(frames: u32, rate: u32) -> u32 {
    frames * 1000 / rate.max(1)
}

/// Runs the stream the settings describe, with the live view, until Escape or an error; then says how it ended.
fn run_stream(settings: &Settings) {
    let real = tape_rt::api();
    let (mode, options) = match build(settings) {
        Ok(found) => found,
        Err(reason) => {
            draw_message(reason);
            return;
        }
    };
    let run = run_state();
    *run = blank_run();
    run.started = tape_rt::micros();
    run.nic = options.nic;
    // One meter row per channel: the ones sent (labelled with the device input they come from), then the ones played.
    let mut add = |channels: Option<ChannelSet>, fallback: usize, run: &mut Run| {
        let numbers: [u8; stats::MAX_CHANNELS] = {
            let mut list = [0u8; stats::MAX_CHANNELS];
            if let Some(set) = channels {
                for (index, channel) in set.channels().enumerate().take(stats::MAX_CHANNELS) {
                    list[index] = channel as u8 + 1;
                }
            }
            list
        };
        let count = channels.map_or(fallback, |set| set.count()).min(stats::MAX_CHANNELS);
        for index in 0..count {
            run.labels[run.rows] = if numbers[index] != 0 { numbers[index] } else { index as u8 + 1 };
            run.rows += 1;
        }
        count
    };
    if mode != "recv" {
        run.sends = add(ChannelSet::parse(settings.send.as_str()), 2, run);
    }
    if mode != "send" {
        add(ChannelSet::parse(settings.receive.as_str()), 2, run);
    }
    stats::reset();
    at(1, 1);
    draw!("\x1b[2J");
    at(1, 1);
    draw!(" fos audio-net: {} ({})\x1b[K", MODES[settings.mode], CODECS[settings.codec]);
    at(2, 1);
    let peer = settings.peer.as_str();
    draw!(" Peer: {}   Network: {}\x1b[K", if peer.is_empty() { "-" } else { peer }, NETWORKS[settings.network]);
    at(3, 1);
    let mut mac = [0u8; 6];
    (real.nic_mac)(options.nic, mac.as_mut_ptr());
    run.mac = mac;
    draw!(" NIC {}  MAC {}  IP {}\x1b[K", options.nic + 1, MacText(mac), if !settings.rtp() { "(layer 2 only)" } else if settings.dhcp { "(asking the DHCP server...)" } else { settings.ip.as_str() });
    draw_scale(run);
    tape_rt::set_quiet(true);
    let shim = abort::wrap_with(real, panel_hook);
    let mut run_options = options.clone();
    let ok = crate::resolve_names(shim, mode, &mut run_options);
    if ok {
        run_once(shim, mode, &run_options);
    }
    tape_rt::set_quiet(false);
}

/// The name this machine announces under: the configured one or `fos-` and the end of the MAC.
fn effective_name(settings: &Settings) -> Text<24> {
    let mut mac = [0u8; 6];
    (tape_rt::api().nic_mac)(settings.nic.saturating_sub(1), mac.as_mut_ptr());
    let mut buffer = [0u8; sap::MAX_NAME];
    Text::from(peers::session_name(settings.name.as_str(), mac, &mut buffer))
}

/// How long the panel waits after the last change before it (re)starts announcing: typing must not thrash it.
const PRESENCE_DELAY: u64 = 1_200_000;
/// An announcement not heard for this long means the machine went away (they repeat every 5 s).
const PRESENCE_EXPIRY: u64 = 20_000_000;

enum Link {
    Off,
    Dhcp(DhcpTask),
    Up,
    Failed(&'static str, u64),
}

/// The panel's presence on the network while it sits on the form: it announces this machine (name, address, what it
/// would listen for) and listens to everybody else's announcements, so the user can pick a peer by name before any
/// stream exists. It needs an IP address, so it runs only for the RTP network; it never blocks (DHCP is stepped).
struct Presence {
    link: Link,
    transport: Option<Transport>,
    igmp: Igmp,
    directory: peers::Directory,
    /// Not before this time (micros): the settings were just changed.
    start_at: u64,
    status: Text<100>,
    shown: Text<100>,
}

impl Presence {
    fn new() -> Presence {
        Presence { link: Link::Off, transport: None, igmp: Igmp::new(sap::GROUP), directory: peers::Directory::new(), start_at: tape_rt::micros() + PRESENCE_DELAY, status: Text::new(), shown: Text::new() }
    }

    /// Withdraws the announcement and goes back to Off (the settings changed, or a stream is about to start).
    fn stop(&mut self, forget_address: bool) {
        let api = tape_rt::api();
        if let Some(mut transport) = self.transport.take() {
            transport.announce_stop(api, transport_nic(&transport));
            self.igmp.leave(api, NIC_IN_USE.get(), &transport.config);
        }
        if forget_address {
            crate::forget_config();
        }
        self.link = Link::Off;
        self.directory = peers::Directory::new();
        self.start_at = tape_rt::micros() + PRESENCE_DELAY;
    }

    fn machines(&self) -> usize {
        let mut names: [Text<24>; 24] = [Text::new(); 24];
        let mut count = 0;
        for entry in self.directory.iter() {
            let name = Text::<24>::from(entry.session.name());
            if !names[..count].contains(&name) && count < 24 {
                names[count] = name;
                count += 1;
            }
        }
        count
    }

    /// One non-blocking pass: advance DHCP, announce when due, read what is waiting. Call it whenever idle.
    fn tick(&mut self, settings: &Settings) {
        let api = tape_rt::api();
        let now = tape_rt::micros();
        if !settings.rtp() {
            self.status = Text::from("Finding peers needs the RTP network (the announcements travel over IP).");
            return;
        }
        match &mut self.link {
            Link::Off => {
                if now < self.start_at {
                    self.status = Text::from("Starting to look for other machines...");
                    return;
                }
                let mut forced = *settings;
                forced.network = 0;
                let Ok((_, options)) = build(&forced) else {
                    self.status = Text::from("Fix the settings above to find peers.");
                    return;
                };
                NIC_IN_USE.set(options.nic);
                let mut mac = [0u8; 6];
                (api.nic_mac)(options.nic, mac.as_mut_ptr());
                if settings.dhcp {
                    self.link = Link::Dhcp(DhcpTask::new(api, options.nic, MacAddress(mac)));
                } else {
                    match net::static_config(MacAddress(mac), settings.ip.as_str(), settings.gateway.as_str()) {
                        Ok(config) => self.bring_up(settings, &options, mac, config),
                        Err(reason) => self.link = Link::Failed(reason, now + 5_000_000),
                    }
                }
            }
            Link::Dhcp(task) => match task.step(api) {
                DhcpStep::Waiting => self.status = Text::from("Getting an address from the DHCP server..."),
                DhcpStep::Failed => self.link = Link::Failed("No DHCP answer: check the cable and the network, or set Addressing to Static.", now + 10_000_000),
                DhcpStep::Bound(config) => {
                    let mut forced = *settings;
                    forced.network = 0;
                    if let Ok((_, options)) = build(&forced) {
                        let mac = config.mac.0;
                        self.bring_up(settings, &options, mac, config);
                    } else {
                        self.link = Link::Off;
                    }
                }
            },
            Link::Failed(reason, retry) => {
                self.status = Text::from(reason);
                if now >= *retry {
                    self.link = Link::Off;
                    self.start_at = now;
                }
            }
            Link::Up => self.poll(settings),
        }
    }

    fn bring_up(&mut self, settings: &Settings, options: &Options<'static>, mac: [u8; 6], config: net::NetConfig) {
        let api = tape_rt::api();
        let mut transport = Transport::from_config(options, mac, config);
        let mut buffer = [0u8; sap::MAX_NAME];
        let name = peers::session_name(settings.name.as_str(), mac, &mut buffer);
        let mut text = [0u8; 900];
        let length = peers::receiver_sdp(options, &config, name, true, &mut text);
        transport.announcer.start(&text[..length]);
        self.igmp = Igmp::new(sap::GROUP);
        self.igmp.join(api, options.nic, &config);
        self.transport = Some(transport);
        self.link = Link::Up;
    }

    fn poll(&mut self, settings: &Settings) {
        let api = tape_rt::api();
        let nic = NIC_IN_USE.get();
        let Some(transport) = self.transport.as_mut() else { return };
        let now = tape_rt::micros();
        transport.announce_service(api, nic);
        self.igmp.poll(api, nic, &transport.config, now);
        let mut frame = [0u8; 1514];
        for _ in 0..8 {
            let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
            if length == 0 {
                break;
            }
            if self.igmp.handle(&frame[..length], now) {
                continue;
            }
            let ticks = (api.ticks)() as u32;
            if let crate::net::Inbound::Udp { source, payload, .. } = net::inbound(api, nic, &transport.config, &mut transport.arp, &frame[..length], sap::PORT..=sap::PORT, ticks) {
                if source != transport.config.ip {
                    if let Some(packet) = sap::parse(payload) {
                        self.directory.update(&packet, frame[6..12].try_into().unwrap_or([0; 6]));
                    }
                }
            }
        }
        self.directory.expire(PRESENCE_EXPIRY);
        let ip = transport.config.ip.0;
        let mut status = Text::<100>::new();
        let others = self.machines();
        let _ = write!(status, "Announcing as {}  ({}.{}.{}.{})   {} other machine{} seen", effective_name(settings).as_str(), ip[0], ip[1], ip[2], ip[3], others, if others == 1 { "" } else { "s" });
        self.status = status;
    }
}

/// The NIC the presence runs on (a global because `Transport` does not carry it).
struct NicCell(core::cell::Cell<usize>);
unsafe impl Sync for NicCell {}
impl NicCell {
    fn get(&self) -> usize {
        self.0.get()
    }
    fn set(&self, nic: usize) {
        self.0.set(nic)
    }
}
static NIC_IN_USE: NicCell = NicCell(core::cell::Cell::new(0));

fn transport_nic(_transport: &Transport) -> usize {
    NIC_IN_USE.get()
}

fn draw_presence_status(presence: &mut Presence) {
    if presence.status != presence.shown {
        at(5 + ROWS + 3, 1);
        draw!(" {}\x1b[K", presence.status.as_str());
        presence.shown = presence.status;
    }
}

/// One machine in the picker: everything announced under one name and address.
#[derive(Clone, Copy)]
struct Machine {
    name: Text<24>,
    ip: [u8; 4],
    mac: [u8; 6],
    sends: bool,
    listens: bool,
    idle: bool,
    channels: usize,
}

fn machines(presence: &Presence) -> ([Machine; 9], usize) {
    let blank = Machine { name: Text::new(), ip: [0; 4], mac: [0; 6], sends: false, listens: false, idle: false, channels: 0 };
    let mut rows = [blank; 9];
    let mut count = 0;
    for entry in presence.directory.iter() {
        let name = Text::<24>::from(entry.session.name());
        let at = rows[..count].iter().position(|row| row.name == name && row.ip == entry.from_ip).or_else(|| {
            if count < 9 {
                rows[count] = Machine { name, ip: entry.from_ip, mac: entry.from_mac, ..blank };
                count += 1;
                Some(count - 1)
            } else {
                None
            }
        });
        if let Some(at) = at {
            let row = &mut rows[at];
            if entry.session.receive_only {
                row.listens = true;
                row.idle |= entry.session.idle;
            } else {
                row.sends = true;
            }
            row.channels = row.channels.max(entry.session.channels);
        }
    }
    (rows, count)
}

/// P: the machines announced on the network (live, from the presence), picked by number.
fn pick_peer(settings: &mut Settings, presence: &mut Presence) -> &'static str {
    if !settings.rtp() {
        return "Finding peers works over IP: set the network to RTP first.";
    }
    let mut decoder = KeyDecoder::new();
    let mut shown: Option<(usize, usize)> = None;
    draw!("\x1b[2J");
    loop {
        presence.tick(settings);
        let (rows, count) = machines(presence);
        let summary = (count, rows[..count].iter().map(|row| usize::from(row.sends) + 2 * usize::from(row.listens) + 4 * usize::from(row.idle)).sum::<usize>());
        if shown != Some(summary) {
            shown = Some(summary);
            draw!("\x1b[2J");
            at(1, 1);
            draw!(" fos audio-net: machines on the network\x1b[K");
            at(2, 1);
            draw!(" This machine: {}   {}\x1b[K", effective_name(settings).as_str(), presence.status.as_str());
            for (index, row) in rows[..count].iter().enumerate() {
                let state = if row.sends && row.listens && !row.idle { "sending and listening" } else if row.sends { "sending" } else if row.idle { "idle" } else { "listening" };
                at(4 + index, 1);
                draw!(" {}  {:<24} {}.{}.{}.{} {}  {}\x1b[K", index + 1, row.name.as_str(), row.ip[0], row.ip[1], row.ip[2], row.ip[3], MacText(row.mac), state);
            }
            if count == 0 {
                at(4, 1);
                draw!(" No other machine heard yet. Other machines show up here while their panel is open or while they stream");
                at(5, 1);
                draw!(" over RTP, on the same switch or network. Give it a few seconds; this list updates by itself.");
            }
            at(15, 1);
            draw!(" Press a number to choose a peer, any other key to go back.");
        }
        match next_key(&mut decoder) {
            Some(Key::Char(digit @ b'1'..=b'9')) => {
                let index = usize::from(digit - b'1');
                if index < count {
                    settings.peer = rows[index].name;
                    return "Peer chosen.";
                }
                return "";
            }
            Some(_) => return "",
            None => tape_rt::idle(),
        }
    }
}

/// What to bring up to date on the screen after a batch of keys. The serial line is slow (every byte costs time, and
/// a serial receiver has a 16-byte FIFO), so a keystroke redraws only the rows it touched.
struct Dirty {
    full: bool,
    rows: u16,
    info: bool,
}

impl Dirty {
    fn row(&mut self, row: usize) {
        self.rows |= 1 << row;
        self.info = true;
    }
}

/// The panel: runs until the user quits.
pub fn run(real: &'static TapeApi) {
    let _ = real;
    let mut settings = Settings::new();
    load_settings(&mut settings);
    let mut selected = 0usize;
    let mut editing = false;
    let mut message = "";
    let mut decoder = KeyDecoder::new();
    let mut info = read_audio_info();
    let mut presence = Presence::new();
    draw_form(&settings, selected, editing, &info, message);
    let mut quit = false;
    while !quit {
        let mut dirty = Dirty { full: false, rows: 0, info: false };
        let mut changed = false; // a setting changed (the announcement must be rebuilt)
        let mut network_changed = false; // the address may have changed too
        let mut any = false;
        // Handle every key that is waiting before drawing once: typing quickly must not lose keys to the drawing.
        while let Some(key) = next_key(&mut decoder) {
            any = true;
            if editing {
                let row = selected;
                if let Some(field) = settings.text_mut(row) {
                    match key {
                        Key::Enter => editing = false,
                        Key::Backspace => field.pop(),
                        Key::Char(byte) => field.push(byte),
                        ref other if is_escape(other) => {
                            editing = false;
                            field.clear();
                        }
                        _ => {}
                    }
                } else {
                    editing = false;
                }
                dirty.row(row);
                changed = true;
                network_changed |= matches!(row, R_IP | R_GATEWAY);
                continue;
            }
            if !message.is_empty() {
                message = "";
                dirty.info = true;
            }
            let before = selected;
            // The direction of travel, so that skipping a row that does not apply continues the same way.
            let mut direction = 1;
            match key {
                Key::Up | Key::Char(b'k') => {
                    selected = (selected + ROWS - 1) % ROWS;
                    direction = ROWS - 1;
                }
                Key::Down | Key::Char(b'j') | Key::Tab => selected = (selected + 1) % ROWS,
                Key::Left | Key::Char(b'h') | Key::Right | Key::Char(b'l') | Key::Char(b' ') => {
                    let step = if matches!(key, Key::Left | Key::Char(b'h')) { -1 } else { 1 };
                    cycle(&mut settings, selected, step);
                    dirty.row(selected);
                    changed = true;
                    network_changed |= matches!(selected, R_NETWORK | R_NIC | R_ADDRESSING);
                    // Another mode or network changes which rows apply.
                    dirty.full |= matches!(selected, R_MODE | R_NETWORK | R_ADDRESSING);
                }
                Key::Enter => {
                    if settings.applies(selected) && settings.text_mut(selected).is_some() {
                        editing = true;
                    } else {
                        cycle(&mut settings, selected, 1);
                        dirty.full |= matches!(selected, R_MODE | R_NETWORK | R_ADDRESSING);
                        changed = true;
                        network_changed |= matches!(selected, R_NETWORK | R_NIC | R_ADDRESSING);
                    }
                    dirty.row(selected);
                }
                Key::Char(b's') | Key::Char(b'S') => {
                    presence.stop(false); // the stream announces itself from here on
                    run_stream(&settings);
                    // Back from the live view: show how it ended, then wait for a key.
                    at(23, 2);
                    let reason = tape_rt::last_message();
                    draw!("Stopped. {reason}\x1b[K");
                    at(24, 2);
                    draw!("Press any key for the settings.\x1b[K");
                    while next_key(&mut decoder).is_none() {
                        tape_rt::idle();
                    }
                    info = read_audio_info();
                    dirty.full = true;
                }
                Key::Char(b'p') | Key::Char(b'P') => {
                    message = pick_peer(&mut settings, &mut presence);
                    dirty.full = true;
                    changed = true;
                }
                Key::Char(b'w') | Key::Char(b'W') => {
                    message = save_settings(&settings);
                    dirty.info = true;
                }
                Key::Char(b'b') | Key::Char(b'B') => {
                    message = toggle_boot(&settings);
                    dirty.info = true;
                }
                Key::Char(b'q') | Key::Char(b'Q') => quit = true,
                ref other if is_escape(other) => quit = true,
                _ => {}
            }
            if selected != before {
                dirty.row(before);
                dirty.row(selected);
            }
            if !settings.applies(selected) {
                // Landed on a row that does not apply (after changing the mode or the network): move to the next that does.
                for step in 1..ROWS {
                    let candidate = (selected + step * direction) % ROWS;
                    if settings.applies(candidate) {
                        dirty.row(selected);
                        selected = candidate;
                        dirty.row(selected);
                        break;
                    }
                }
            }
        }
        if quit {
            break;
        }
        if changed && !editing {
            presence.stop(network_changed);
        }
        if !any {
            presence.tick(&settings);
            draw_presence_status(&mut presence);
            tape_rt::idle();
            continue;
        }
        if dirty.full {
            draw_form(&settings, selected, editing, &info, message);
            presence.shown = Text::new(); // the full redraw wiped the status line
        } else {
            for row in 0..ROWS {
                if dirty.rows & (1 << row) != 0 {
                    draw_row(&settings, row, selected, editing, &info);
                }
            }
            if dirty.info {
                draw_info(&settings, message);
            }
        }
        draw_presence_status(&mut presence);
    }
    presence.stop(false);
    at(1, 1);
    draw!("\x1b[2J");
    at(1, 1);
    draw!("audio-net: left the panel.\n");
}
