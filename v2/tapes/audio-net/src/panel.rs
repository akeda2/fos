//! The settings panel: `tape run audio-net` with no arguments. A full-screen form for the things a user decides
//! (mode, network, peer, codec, channels), a peer picker fed by the announcements, and a live view of the running
//! stream with level meters. It draws with the few ANSI sequences the console and every terminal know (clear, cursor
//! position, erase to end of line) and reads keys through `read_key`, so it works on the local screen and over `term
//! net` alike. Settings are loaded from and saved to `audio.cfg` (the same file `audio-net @audio.cfg` runs), and the
//! panel can write the `start` file that launches it at boot.

use crate::abort;
use crate::stats;
use crate::{MacText, Options, parse, peers, run_once};
use audio_wire::{ChannelSet, Cursor};
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
                let _ = write!(out, "(automatic: fos- and the MAC)");
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
    draw!(" fos audio-net: settings\x1b[K");
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

/// The live view's state, shared with the hook that draws it from inside the stream loops.
struct Run {
    stop: bool,
    next_draw: u64,
    started: u64,
    send_channels: usize,
    receive_channels: usize,
    /// The address line has been brought up to date with the address a DHCP lease gave.
    ip_shown: bool,
    mac: [u8; 6],
    nic: usize,
    counters: Text<100>,
    message: Text<100>,
    meters: [i8; 2 * stats::MAX_CHANNELS],
}

static mut RUN: Run = Run { stop: false, next_draw: 0, started: 0, send_channels: 0, receive_channels: 0, ip_shown: false, mac: [0; 6], nic: 0, counters: Text::new(), message: Text::new(), meters: [-1; 2 * stats::MAX_CHANNELS] };

fn run_state() -> &'static mut Run {
    unsafe { &mut *(&raw mut RUN) }
}

const METER_ROW: usize = 9;

/// Where meter `index` of a group (0 send, 1 receive) is drawn: four to a row.
fn meter_position(group: usize, index: usize, send_rows: usize) -> (usize, usize) {
    let base = METER_ROW + if group == 0 { 0 } else { send_rows + 1 };
    (base + index / 4, 2 + (index % 4) * 19)
}

fn draw_meters(group: usize, count: usize, send_rows: usize, peaks: &mut [u32; stats::MAX_CHANNELS], cache: &mut [i8]) {
    for index in 0..count.min(stats::MAX_CHANNELS) {
        let steps = stats::steps_below_full(peaks[index]);
        peaks[index] = 0;
        let cells = ((20 - steps) / 2) as i8;
        if cache[group * stats::MAX_CHANNELS + index] == cells {
            continue;
        }
        cache[group * stats::MAX_CHANNELS + index] = cells;
        let (row, column) = meter_position(group, index, send_rows);
        at(row, column);
        draw!("{:>2} [", index + 1);
        for cell in 0..10 {
            draw!("{}", if cell < cells { '#' } else { '.' });
        }
        draw!("]");
    }
}

/// Called from inside the stream loops (as `should_abort`): reads a key, and a couple of times a second brings the
/// counters, the last message and the meters up to date. Only what changed is sent, because every byte costs time
/// the stream needs.
extern "C" fn panel_hook() -> u32 {
    let run = run_state();
    if run.stop {
        return 1;
    }
    let event = (tape_rt::api().read_key)();
    if event == 1 || event == 0x100 + u32::from(b'q') || event == 0x100 + u32::from(b'Q') {
        run.stop = true;
        return 1;
    }
    let now = tape_rt::micros();
    if now < run.next_draw {
        return 0;
    }
    run.next_draw = now + 250_000;
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
    let _ = write!(counters, "Sent {}   Received {}   Lost {}   Ring {} ms (target {})   Clock {:+} ppm   {}:{:02}", shown.sent, shown.received, shown.lost, ms(shown.level_frames, shown.rate), ms(shown.target_frames, shown.rate), shown.offset_ppm, seconds / 60, seconds % 60);
    if counters != run.counters {
        at(5, 2);
        draw!("{}\x1b[K", counters.as_str());
        run.counters = counters;
    }
    let message = Text::<100>::from(tape_rt::last_message());
    if message != run.message {
        at(6, 2);
        draw!("{}\x1b[K", message.as_str());
        run.message = message;
    }
    let send_rows = run.send_channels.min(stats::MAX_CHANNELS).div_ceil(4);
    let (send_channels, receive_channels) = (run.send_channels, run.receive_channels);
    draw_meters(0, send_channels, send_rows, &mut shown.send_peaks, &mut run.meters);
    draw_meters(1, receive_channels, send_rows, &mut shown.receive_peaks, &mut run.meters);
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
    *run = Run { stop: false, next_draw: 0, started: tape_rt::micros(), send_channels: 0, receive_channels: 0, ip_shown: false, mac: [0; 6], nic: options.nic, counters: Text::new(), message: Text::new(), meters: [-1; 2 * stats::MAX_CHANNELS] };
    if mode != "recv" {
        run.send_channels = ChannelSet::parse(settings.send.as_str()).map_or(2, |set| set.count());
    }
    if mode != "send" {
        run.receive_channels = ChannelSet::parse(settings.receive.as_str()).map_or(2, |set| set.count());
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
    at(4, 1);
    draw!(" {:-<78}", "");
    let send_rows = run.send_channels.min(stats::MAX_CHANNELS).div_ceil(4);
    if run.send_channels > 0 {
        at(METER_ROW - 1, 2);
        draw!("Sending, peak level per channel (6 dB per step)");
    }
    if run.receive_channels > 0 {
        at(METER_ROW + if run.send_channels > 0 { send_rows + 1 } else { 0 } - 1, 2);
        draw!("Receiving, peak level per channel");
    }
    let bottom = METER_ROW + send_rows + run.receive_channels.min(stats::MAX_CHANNELS).div_ceil(4) + 2;
    at(bottom.min(23), 2);
    draw!("Esc or Q: stop and return to the settings");
    tape_rt::set_quiet(true);
    let shim = abort::wrap_with(real, panel_hook);
    let mut run_options = options.clone();
    let ok = crate::resolve_names(shim, mode, &mut run_options);
    if ok {
        run_once(shim, mode, &run_options);
    }
    tape_rt::set_quiet(false);
}

/// P: listens for announcements and lets the user pick a peer by number.
fn pick_peer(settings: &mut Settings) -> &'static str {
    let mut forced = *settings;
    forced.network = 0;
    let Ok((_, options)) = build(&forced) else { return "Fill in a valid IP address first (the announcements travel over IP)." };
    if !settings.rtp() {
        return "Announcements travel over IP: set the network to RTP first.";
    }
    at(5 + ROWS + 2, 1);
    draw!(" Listening for announcements for 8 s (Esc stops)...\x1b[K");
    tape_rt::set_quiet(true);
    let real = tape_rt::api();
    let Some(mut transport) = crate::Transport::open(real, &options) else {
        tape_rt::set_quiet(false);
        return "Could not set up the network (no DHCP lease?).";
    };
    let mut short = options.clone();
    short.seconds = 8;
    let directory = peers::listen(real, &short, &mut transport, 8, |_| false);
    tape_rt::set_quiet(false);
    // Senders are what a receiver wants, listeners what a sender wants; a duplex machine takes either.
    let wanted = |entry: &peers::Entry| match settings.mode {
        0 => entry.session.receive_only,
        1 => !entry.session.receive_only,
        _ => true,
    };
    let mut names: [Text<24>; 9] = [Text::new(); 9];
    let mut count = 0;
    at(1, 1);
    draw!("\x1b[2J");
    at(1, 1);
    draw!(" fos audio-net: announced peers\x1b[K");
    for entry in directory.iter().filter(|entry| wanted(entry)) {
        if count < 9 {
            names[count] = Text::from(entry.session.name());
            let (ip, s) = (entry.from_ip, &entry.session);
            at(3 + count, 1);
            draw!(" {}  {:<24} {:<8} {}.{}.{}.{} {}  {} ch\x1b[K", count + 1, s.name(), if s.receive_only { "listens" } else { "sends" }, ip[0], ip[1], ip[2], ip[3], MacText(entry.from_mac), s.channels);
            count += 1;
        }
    }
    if count == 0 {
        at(3, 1);
        draw!(" Nothing announced for this mode. A peer announces itself while it streams over RTP.");
    }
    at(14, 1);
    draw!(" Press a number to choose, any other key to go back.");
    let mut decoder = KeyDecoder::new();
    loop {
        if let Some(key) = next_key(&mut decoder) {
            if let Key::Char(digit @ b'1'..=b'9') = key {
                let index = usize::from(digit - b'1');
                if index < count {
                    settings.peer = names[index];
                    return "Peer chosen.";
                }
            }
            return "";
        }
        tape_rt::idle();
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
    draw_form(&settings, selected, editing, &info, message);
    let mut quit = false;
    while !quit {
        let mut dirty = Dirty { full: false, rows: 0, info: false };
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
                continue;
            }
            if !message.is_empty() {
                message = "";
                dirty.info = true;
            }
            let before = selected;
            match key {
                Key::Up | Key::Char(b'k') => selected = (selected + ROWS - 1) % ROWS,
                Key::Down | Key::Char(b'j') | Key::Tab => selected = (selected + 1) % ROWS,
                Key::Left | Key::Char(b'h') | Key::Right | Key::Char(b'l') | Key::Char(b' ') => {
                    let step = if matches!(key, Key::Left | Key::Char(b'h')) { -1 } else { 1 };
                    cycle(&mut settings, selected, step);
                    dirty.row(selected);
                    // Another mode or network changes which rows apply.
                    dirty.full |= matches!(selected, R_MODE | R_NETWORK | R_ADDRESSING);
                }
                Key::Enter => {
                    if settings.applies(selected) && settings.text_mut(selected).is_some() {
                        editing = true;
                    } else {
                        cycle(&mut settings, selected, 1);
                        dirty.full |= matches!(selected, R_MODE | R_NETWORK | R_ADDRESSING);
                    }
                    dirty.row(selected);
                }
                Key::Char(b's') | Key::Char(b'S') => {
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
                    message = pick_peer(&mut settings);
                    dirty.full = true;
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
                    let candidate = (selected + step) % ROWS;
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
        if !any {
            tape_rt::idle();
            continue;
        }
        if dirty.full {
            draw_form(&settings, selected, editing, &info, message);
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
    }
    at(1, 1);
    draw!("\x1b[2J");
    at(1, 1);
    draw!("audio-net: left the panel.\n");
}
