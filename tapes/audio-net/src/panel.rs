//! The settings panel: `tape run audio-net` with no arguments. A full-screen form for the things a user decides
//! (mode, network, peer, codec, channels), a peer picker fed by the announcements, and a live view of the running
//! stream with level meters. It draws with the few ANSI sequences the console and every terminal know (clear, cursor
//! position, erase to end of line) and reads keys through `read_key`, so it works on the local screen and over `term
//! net` alike. Settings are loaded from and saved to `audio.cfg` (the same file `audio-net @audio.cfg` runs), and the
//! panel can write the `start` file that launches it at boot.

use crate::igmp::Igmp;
use crate::net::{self, DhcpStep, DhcpTask};
use crate::stats;
use audio_wire::sap;
use tape_net::MacAddress;
use crate::{MacText, Options, Transport, parse, peers, run_once};
use audio_wire::{ChannelSet, Cursor};
use core::fmt::Write;
use tape_panel::meters::MeterBank;
use tape_panel::{Text, at, begin_batch, draw, end_batch};
use tape_rt::{AudioInfo, Key, KeyDecoder, TapeApi};

const CONFIG_FILE: &str = "audio.cfg";
const START_FILE: &str = "start";
const START_LINE: &str = "run audio-net @audio.cfg";

const MODES: [&str; 4] = ["Send", "Receive", "Duplex (send and receive)", "Serve (Ogg Opus over HTTP, for a browser or VLC)"];
/// `Settings::mode` of the HTTP server.
const SERVE: usize = 3;
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
const R_PORT: usize = 12;
const R_PREBUFFER: usize = 13;
const ROWS: usize = 14;
const LABELS: [&str; ROWS] = ["Mode", "Network", "NIC", "Addressing", "IP address", "Gateway", "Peer", "Codec (sent)", "Send channels", "Receive channels", "Name", "Multicast group", "HTTP port", "Pre-buffer"];

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
    /// `serve`: the TCP port; empty means the default, 8000.
    port: Text<6>,
    /// `serve`: milliseconds of audio sent ahead to a new client; empty or 0 = off.
    prebuffer: Text<6>,
}

impl Settings {
    fn new() -> Settings {
        Settings { mode: 0, network: 0, nic: 1, codec: 0, dhcp: true, ip: Text::from("192.168.1.20/24"), gateway: Text::new(), peer: Text::new(), name: Text::new(), group: Text::new(), send: Text::from("1-2"), receive: Text::from("1-2"), port: Text::new(), prebuffer: Text::new() }
    }

    /// The network needs an IP address: RTP, or the HTTP server (which has no other network).
    fn rtp(&self) -> bool {
        self.network == 0 || self.mode == SERVE
    }

    fn applies(&self, row: usize) -> bool {
        match row {
            R_NETWORK | R_PEER => self.mode != SERVE,
            R_NAME | R_GROUP => self.rtp() && self.mode != SERVE,
            R_ADDRESSING => self.rtp(),
            R_IP | R_GATEWAY => self.rtp() && !self.dhcp,
            R_SEND => self.mode != 1,
            R_RECEIVE => self.mode == 1 || self.mode == 2,
            R_PORT | R_PREBUFFER => self.mode == SERVE,
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
            R_PORT => Some(&mut self.port),
            R_PREBUFFER => Some(&mut self.prebuffer),
            _ => None,
        }
    }

    /// The settings as a configuration file (`audio-net @audio.cfg` runs it).
    fn cfg(&self, out: &mut Cursor<'_>, for_run: bool) {
        let _ = writeln!(out, "# audio-net configuration, written by the panel");
        let _ = writeln!(out, "mode {}", ["send", "recv", "duplex", "serve"][self.mode]);
        let _ = writeln!(out, "proto {}", ["rtp", "raw", "avtp"][if self.mode == SERVE { 0 } else { self.network }]);
        // By MAC address: the NIC numbers change when cards are moved, the addresses do not.
        let mut mac = [0u8; 6];
        (tape_rt::api().nic_mac)(self.nic - 1, mac.as_mut_ptr());
        let _ = writeln!(out, "nic {}", MacText(mac));
        if self.rtp() {
            if self.dhcp {
                let _ = writeln!(out, "ip dhcp");
            }
            for (key, value) in [("ip", if self.dhcp { "" } else { self.ip.as_str() }), ("gw", if self.dhcp { "" } else { self.gateway.as_str() }), ("name", if self.mode == SERVE { "" } else { self.name.as_str() }), ("group", if self.mode == SERVE { "" } else { self.group.as_str() })] {
                if !value.is_empty() {
                    let _ = writeln!(out, "{key} {value}");
                }
            }
        }
        let peer = if self.mode == SERVE { "" } else { self.peer.as_str() };
        if peer.is_empty() && self.mode == 1 && self.rtp() && self.group.is_empty() {
            let _ = writeln!(out, "source auto"); // the one stream that is announced
        }
        if !peer.is_empty() {
            if self.mode != 1 {
                let _ = writeln!(out, "to {peer}");
            }
            if self.mode != 0 {
                // A name is looked up in the announcements (`source`); on a layer 2 network a typed MAC address is `from`.
                let is_mac = !self.rtp() && crate::parse_mac(peer).is_some();
                let _ = writeln!(out, "{} {peer}", if is_mac { "from" } else { "source" });
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
        if self.mode == SERVE && !self.port.is_empty() {
            let _ = writeln!(out, "http {}", self.port.as_str());
        }
        if self.mode == SERVE && !self.prebuffer.is_empty() && self.prebuffer.as_str() != "0" {
            let _ = writeln!(out, "prebuffer {}", self.prebuffer.as_str());
        }
        match self.mode {
            0 | SERVE => {
                let _ = writeln!(out, "ch {}", self.send.as_str());
            }
            1 => {
                let _ = writeln!(out, "ch {}", self.receive.as_str());
            }
            _ => {
                let _ = writeln!(out, "in {}\nout {}", self.send.as_str(), self.receive.as_str());
            }
        }
        if self.mode == 1 || self.mode == 2 {
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
            "serve" => Some(SERVE),
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
                "nic" => self.nic = match crate::parse_mac(value) {
                    Some(mac) => crate::nic_by_mac(mac).map_or(1, |index| index + 1), // a card that is gone: back to the first
                    None => value.parse().unwrap_or(1),
                },
                "ip" if value == "dhcp" => self.dhcp = true,
                "ip" => (self.dhcp, self.ip) = (false, Text::from(value)),
                "gw" => self.gateway = Text::from(value),
                "name" => self.name = Text::from(value),
                "group" => self.group = Text::from(value),
                "http" => self.port = Text::from(value),
                "prebuffer" => self.prebuffer = Text::from(value),
                "to" | "source" | "from" if value == "auto" => {}
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
        if self.mode == SERVE && self.codec < 2 {
            self.codec = 2; // Ogg Opus: there is no PCM here
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
        Text::clear(self);
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
    if settings.applies(R_GROUP) && !settings.group.is_empty() && !crate::net::parse_ip(settings.group.as_str()).is_some_and(|group| audio_wire::is_multicast_address(group.0)) {
        return Err("Multicast group: an address such as 239.1.2.3 (224.0.0.0-239.255.255.255), or empty for automatic.");
    }
    let text = cfg_text(settings, true);
    let mut words: [&'static str; crate::MAX_WORDS] = [""; crate::MAX_WORDS];
    let mut count = 0;
    crate::normalize(text, &mut words, &mut count)?;
    let mut mode = "";
    let mut kept: [&'static str; crate::MAX_WORDS] = [""; crate::MAX_WORDS];
    let mut kept_count = 0;
    for &word in &words[..count] {
        match word.strip_prefix("mode=").unwrap_or(word) {
            "send" | "recv" | "duplex" | "serve" => mode = word.strip_prefix("mode=").unwrap_or(word),
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
    if row == R_NETWORK && settings.mode == SERVE {
        let _ = write!(out, "IP (HTTP needs an address)");
        return;
    }
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
                let _ = write!(out, "{}", match (settings.mode, settings.rtp()) {
                    (1, true) => "-  (the announced stream; P picks one)",
                    (1, false) => "-  (any sender; P picks from the announced ones)",
                    (_, true) => "-  (automatic multicast group; P picks a machine)",
                    _ => "-  (P picks from the announced ones)",
                });
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
        R_PORT => {
            if settings.port.is_empty() {
                let _ = write!(out, "8000  (the default; type a port to change it)");
            } else {
                let _ = write!(out, "{}", settings.port.as_str());
            }
        }
        R_PREBUFFER => {
            if settings.prebuffer.is_empty() || settings.prebuffer.as_str() == "0" {
                let _ = write!(out, "off  (type milliseconds, e.g. 200, to fill the player's buffer at once; adds that delay)");
            } else {
                let _ = write!(out, "{} ms", settings.prebuffer.as_str());
            }
        }
        R_IP => dash(settings.ip.as_str(), out),
        R_GATEWAY => dash(settings.gateway.as_str(), out),
        R_NAME => {
            if settings.name.is_empty() {
                let _ = write!(out, "{}  (automatic: the kernel's host name; type a name to change it)", effective_name(settings).as_str());
            } else {
                let _ = write!(out, "{}", settings.name.as_str());
            }
        }
        _ => {
            if settings.group.is_empty() {
                let _ = write!(out, "-  (automatic: a sender picks one, receivers follow its announcement)");
            } else {
                let _ = write!(out, "{}", settings.group.as_str());
            }
        }
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
        0 | SERVE => per(send),
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
        draw!(" Device: no audio device found (send can use a test tone)\x1b[K");
    } else {
        match tape_rt::active_audio_device(tape_rt::api()) {
            Some(device) => draw!(" Device: {}, {} inputs / {} outputs, {} bit\x1b[K", tape_rt::AudioDeviceName(device), info.capture_channels, info.playback_channels, info.capture_bits),
            None => draw!(" Device: USB audio (class {}.0), {} inputs / {} outputs, {} bit\x1b[K", info.class_version, info.capture_channels, info.playback_channels, info.capture_bits),
        }
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
            R_PORT => settings.port.as_str(),
            R_PREBUFFER => settings.prebuffer.as_str(),
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
        R_MODE => {
            settings.mode = wrap(settings.mode, MODES.len());
            if settings.mode == SERVE && settings.codec < 2 {
                settings.codec = 2; // Ogg Opus: there is no PCM here
            }
        }
        R_ADDRESSING => settings.dhcp = !settings.dhcp,
        R_NETWORK => {
            settings.network = wrap(settings.network, 3);
            if !settings.rtp() && settings.codec >= 2 {
                settings.codec = 0; // Opus needs RTP
            }
        }
        R_NIC => settings.nic = 1 + wrap(settings.nic - 1, ((tape_rt::api().nic_count)()).max(1)),
        R_CODEC => {
            if settings.mode == SERVE {
                // The Opus choices only (2 to 4), wrapping.
                settings.codec = 2 + (settings.codec.max(2) - 2 + if step < 0 { 2 } else { 1 }) % 3;
            } else {
                let count = if settings.rtp() { CODECS.len() } else { 2 };
                settings.codec = wrap(settings.codec, count);
            }
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
const ROW_SCALE: usize = 7;
const ROW_FIRST: usize = 9;
const PAGE_ROWS: usize = 12;
const MAX_ROWS: usize = 2 * stats::MAX_CHANNELS;
const HINT: &str = "U: Nordic/dBFS   R: reset hold   M: meters off/on   Esc or Q: stop and return";

/// The live view's state, shared with the hook that draws it from inside the stream loops.
struct Run {
    stop: bool,
    next_draw: u64,
    started: u64,
    /// One meter row per channel sent, then one per channel received.
    meters: MeterBank<MAX_ROWS>,
    sends: usize,
    next_counters: u64,
    ip_shown: bool,
    /// `serve`: the HTTP port, so that the live view can show the URL once the address is known (0 otherwise).
    serve_port: u16,
    mac: [u8; 6],
    nic: usize,
    counters: Text<100>,
    message: Text<100>,
    actual: Text<100>,
    /// The longest the panel's own drawing has held up the stream loop since the run began (microseconds).
    ui_worst: u64,
    /// When the screen was last drawn (micros): drawing is held back while the playback buffer is low.
    last_draw: u64,
    /// The same over the last ten seconds (so a start-up stall does not hide how it behaves now).
    ui_recent: u64,
    ui_recent_since: u64,
    /// The most audio that has waited in the kernel's capture buffer since the run began, in bytes (sampled at every call of
    /// the hook): on the sending side this is audio that is late, that is latency.
    cap_peak: u64,
    /// Bytes of one captured device frame (all its input channels): what turns the buffer's bytes into milliseconds.
    cap_frame: u64,
}

const fn blank_run() -> Run {
    Run {
        stop: false, next_draw: 0, started: 0, meters: MeterBank::new(ROW_SCALE, ROW_FIRST, PAGE_ROWS), sends: 0, next_counters: 0, ip_shown: false, serve_port: 0, mac: [0; 6], nic: 0,
        counters: Text::new(), message: Text::new(), actual: Text::new(), ui_worst: 0, last_draw: 0, ui_recent: 0, ui_recent_since: 0, cap_peak: 0, cap_frame: 0,
    }
}

static mut RUN: Run = blank_run();

fn run_state() -> &'static mut Run {
    unsafe { &mut *(&raw mut RUN) }
}

/// Called from inside the stream loops (as `should_abort`): reads a key, and ten times a second brings the counters,
/// the last message and the meters up to date. Only what changed is sent, because every byte costs time the stream
/// needs.
extern "C" fn panel_hook() -> u32 {
    // Everything this call draws goes to the console as one write: the console compares its whole text grid after each
    // write, so one write per update holds the stream loop up far less than one per counter line and meter row.
    begin_batch();
    let result = panel_hook_inner();
    end_batch();
    result
}

fn panel_hook_inner() -> u32 {
    let run = run_state();
    if run.stop {
        return 1;
    }
    let event = (tape_rt::api().read_key)();
    if event == 1 {
        run.stop = true;
        return 1;
    }
    {
        let mut waiting = tape_rt::AudioBuffer::default();
        (tape_rt::api().audio_buffer)(&mut waiting);
        run.cap_peak = run.cap_peak.max(waiting.available_bytes);
    }
    if event >= 0x100 {
        match (event & 0xff) as u8 {
            b'q' | b'Q' => {
                run.stop = true;
                return 1;
            }
            b'n' | b'N' => run.meters.next_page(),
            b'b' | b'B' => run.meters.previous_page(),
            b'u' | b'U' => run.meters.toggle_scale(),
            b'r' | b'R' => run.meters.reset_hold(),
            b'm' | b'M' => {
                // Meters off: no detector work per sample and no meter drawing, in case the machine is too slow for
                // them (the counters line shows the longest the drawing has held the stream up).
                let stats = stats::get();
                stats.meters_on = !stats.meters_on;
                if stats.meters_on {
                    run.meters.blank(None);
                    run.meters.mark_all();
                } else {
                    run.meters.blank(Some("Meters are off (M turns them on again)."));
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
    // Drawing holds the stream loop up for a few milliseconds, which is only safe while the playback buffer has some to
    // spare: hold it back while the buffer is low (at most two seconds, so the screen never freezes for long).
    {
        let shown = stats::get();
        let level_ms = ms(shown.level_frames, shown.rate);
        if shown.target_frames != 0 && level_ms < 8 && now < run.last_draw + 2_000_000 {
            return 0;
        }
        run.last_draw = now;
    }
    // One small piece of drawing per call, so that no call holds the stream up for long.
    if stats::get().meters_on && run.meters.take_footer_dirty() {
        run.meters.draw_footer(HINT);
    } else if now >= run.next_counters {
        run.next_counters = now + 250_000;
        if !run.ip_shown {
            if let Some(ip) = crate::current_ip() {
                run.ip_shown = true;
                at(3, 1);
                draw!(" NIC {}  MAC {}  IP {}.{}.{}.{}\x1b[K", run.nic + 1, MacText(run.mac), ip[0], ip[1], ip[2], ip[3]);
                if run.serve_port != 0 {
                    at(2, 1);
                    draw!(" Open http://{}.{}.{}.{}:{}/live.opus (VLC) or http://{}.{}.{}.{}:{}/ (browser)\x1b[K", ip[0], ip[1], ip[2], ip[3], run.serve_port, ip[0], ip[1], ip[2], ip[3], run.serve_port);
                }
            }
        }
        let shown = stats::get();
        let mut counters = Text::<100>::new();
        let seconds = (now - run.started) / 1_000_000;
        let mut device = tape_rt::AudioStats::default();
        (tape_rt::api().audio_stats)(&mut device);
        let _ = write!(
            counters,
            "Tx {} Rx {} Lost {} Ring {}/{}ms {:+}ppm UI {}/{}ms Un {} Ov {} {}:{:02}",
            shown.sent, shown.received, shown.lost, ms(shown.level_frames, shown.rate), ms(shown.target_frames, shown.rate), shown.offset_ppm,
            run.ui_recent / 1000, run.ui_worst / 1000, device.playback_underruns.saturating_sub(shown.underrun_base), device.capture_overruns, seconds / 60, seconds % 60
        );
        if counters != run.counters {
            at(5, 2);
            draw!("{}\x1b[K", counters.as_str());
            run.counters = counters;
        }
        // What is really sent and received: a peer's announcement can replace the choice made on this machine.
        let mut actual = Text::<100>::new();
        if run.serve_port != 0 {
            let _ = write!(actual, "Clients: {}", if shown.serve_status.as_str().is_empty() { "waiting for the first" } else { shown.serve_status.as_str() });
        } else {
            let _ = write!(actual, "Sending: {}   Receiving: {}", if shown.send_codec.as_str().is_empty() { "-" } else { shown.send_codec.as_str() }, if shown.receive_codec.as_str().is_empty() { "-" } else { shown.receive_codec.as_str() });
            if shown.packet_frames != 0 && !shown.receive_codec.as_str().is_empty() {
                let _ = write!(actual, "   Delay ~{} ms", shown.delay_ms());
            }
            // Capture side: audio waiting for the sender to take it is audio that is late. Now and the peak, in ms.
            let mut waiting = tape_rt::AudioBuffer::default();
            (tape_rt::api().audio_buffer)(&mut waiting);
            if waiting.ring_bytes != 0 && run.cap_frame != 0 && shown.rate != 0 {
                let per_ms = (u64::from(shown.rate) * run.cap_frame / 1000).max(1);
                let _ = write!(actual, "   Capture late {}/{} ms", waiting.available_bytes / per_ms, run.cap_peak / per_ms);
            }
        }
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
        let shown = stats::get();
        let sends = run.sends;
        run.meters.update(now, |index| {
            let (detector, clip) = if index < sends { (&mut shown.send[index], &mut shown.send_clip[index]) } else { (&mut shown.receive[index - sends], &mut shown.receive_clip[index - sends]) };
            (detector.take_max(), core::mem::take(clip))
        });
    }
    // How long this call took: the stream loop was not running meanwhile.
    let spent = tape_rt::micros() - now;
    run.ui_worst = run.ui_worst.max(spent);
    if now > run.ui_recent_since + 10_000_000 {
        run.ui_recent_since = now;
        run.ui_recent = 0;
    }
    run.ui_recent = run.ui_recent.max(spent);
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
    {
        let mut device = tape_rt::AudioInfo::default();
        if (real.audio_info)(&mut device) != 0 {
            run.cap_frame = u64::from(device.capture_channels) * u64::from(device.capture_slot_bytes);
        }
    }
    run.nic = options.nic;
    // One meter row per channel: the ones sent (labelled with the device input they come from), then the ones played.
    let add = |channels: Option<ChannelSet>, fallback: usize, sending: bool, run: &mut Run| {
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
            // Labelled by the direction of the stream (what the user is setting up), with the device channel number.
            let number = if numbers[index] != 0 { numbers[index] } else { index as u8 + 1 };
            let mut label = Text::<8>::new();
            let _ = write!(label, "{} {:>2}", if sending { "Send" } else { "Recv" }, number);
            run.meters.add_row(label.as_str());
        }
        count
    };
    if mode != "recv" {
        run.sends = add(ChannelSet::parse(settings.send.as_str()), 2, true, run);
    }
    if mode == "recv" || mode == "duplex" {
        add(ChannelSet::parse(settings.receive.as_str()), 2, false, run);
    }
    stats::reset();
    at(1, 1);
    draw!("\x1b[2J");
    at(1, 1);
    draw!(" fos audio-net: {} ({})\x1b[K", MODES[settings.mode], CODECS[settings.codec]);
    at(2, 1);
    let peer = settings.peer.as_str();
    if settings.mode == SERVE {
        run.serve_port = options.http;
        draw!(" Waiting for the address; the player URL shows here.\x1b[K");
    } else {
        draw!(" Peer: {}   Network: {}\x1b[K", if peer.is_empty() { "-" } else { peer }, NETWORKS[settings.network]);
    }
    at(3, 1);
    let mut mac = [0u8; 6];
    (real.nic_mac)(options.nic, mac.as_mut_ptr());
    run.mac = mac;
    draw!(" NIC {}  MAC {}  IP {}\x1b[K", options.nic + 1, MacText(mac), if !settings.rtp() { "(layer 2 only)" } else if settings.dhcp { "(asking the DHCP server...)" } else { settings.ip.as_str() });
    run.meters.draw_scale();
    tape_rt::set_quiet(true);
    let shim = tape_rt::abort::wrap_with(real, panel_hook);
    // Like a configuration-file run, keep trying until Escape: the peer may not be up yet (a receiver started before
    // its sender waits for the sender's announcement, staying findable meanwhile), or the stream may end when the peer
    // goes away. The last message line says what it is waiting for.
    loop {
        let mut run_options = options.clone();
        // Drawing the screen stalls the stream loop briefly, so the panel plays out with a little more margin (12 ms).
        run_options.target_ms = run_options.target_ms.max(12);
        if crate::resolve_names(shim, mode, &mut run_options) {
            run_once(shim, mode, &run_options);
        }
        if run_state().stop {
            break;
        }
        let until = tape_rt::micros() + 3_000_000;
        while tape_rt::micros() < until && (shim.should_abort)() == 0 {
            tape_rt::idle();
        }
        if run_state().stop {
            break;
        }
    }
    tape_rt::set_quiet(false);
}

/// The name this machine announces under: the configured one or the kernel's host name.
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
            if transport.config.ip.0 != [0; 4] {
                self.igmp.leave(api, NIC_IN_USE.get(), &transport.config);
            }
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
        if settings.mode == SERVE {
            self.status = Text::from("Serve: press S to start; the address and the URL to open are shown then.");
            return;
        }
        if !settings.rtp() {
            // Raw Ethernet and AVTP: no address to get, the announcements are plain Ethernet frames (peers.rs).
            match &self.link {
                Link::Off => {
                    if now < self.start_at {
                        self.status = Text::from("Starting to look for other machines...");
                        return;
                    }
                    let Ok((_, options)) = build(settings) else {
                        self.status = Text::from("Fix the settings above to find peers.");
                        return;
                    };
                    NIC_IN_USE.set(options.nic);
                    let mut mac = [0u8; 6];
                    (api.nic_mac)(options.nic, mac.as_mut_ptr());
                    self.bring_up_l2(settings, &options, mac);
                }
                Link::Up => self.poll(settings),
                _ => self.link = Link::Off,
            }
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

    /// The presence on a layer 2 network: announce that this machine is here, and what it would listen for.
    fn bring_up_l2(&mut self, settings: &Settings, options: &Options<'static>, mac: [u8; 6]) {
        let mut transport = Transport::from_l2(options, mac);
        let mut buffer = [0u8; sap::MAX_NAME];
        let name = peers::session_name(settings.name.as_str(), mac, &mut buffer);
        let mut text = [0u8; 900];
        let length = peers::l2_sdp(options, mac, name, true, true, options.chan, &mut text);
        transport.announcer.start(&text[..length]);
        self.transport = Some(transport);
        self.link = Link::Up;
    }

    /// One pass of the layer 2 presence: announce when due, read the announcements waiting.
    fn poll_l2(&mut self, settings: &Settings) {
        let api = tape_rt::api();
        let nic = NIC_IN_USE.get();
        let Some(transport) = self.transport.as_mut() else { return };
        transport.announce_service(api, nic);
        let mut frame = [0u8; 1514];
        for _ in 0..8 {
            let length = (api.nic_receive)(nic, frame.as_mut_ptr(), frame.len());
            if length == 0 {
                break;
            }
            peers::l2_announcement(&frame[..length], transport.mac, &mut self.directory);
        }
        let mac = transport.mac;
        self.directory.expire(PRESENCE_EXPIRY);
        let mut status = Text::<100>::new();
        let others = self.machines();
        let _ = write!(status, "Announcing as {} (layer 2, MAC {})   {} other machine{} seen", effective_name(settings).as_str(), MacText(mac), others, if others == 1 { "" } else { "s" });
        self.status = status;
    }

    fn poll(&mut self, settings: &Settings) {
        if self.transport.as_ref().is_some_and(|transport| transport.config.ip.0 == [0; 4]) {
            return self.poll_l2(settings);
        }
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
    /// 0 on an IP network, else 1 raw Ethernet or 2 AVTP (the kind of layer 2 network it announced on).
    l2: u8,
}

fn machines(presence: &Presence) -> ([Machine; 9], usize) {
    let blank = Machine { name: Text::new(), ip: [0; 4], mac: [0; 6], sends: false, listens: false, idle: false, channels: 0, l2: 0 };
    let mut rows = [blank; 9];
    let mut count = 0;
    for entry in presence.directory.iter() {
        let name = Text::<24>::from(entry.session.name());
        let at = rows[..count].iter().position(|row| row.name == name && row.ip == entry.from_ip).or_else(|| {
            if count < 9 {
                rows[count] = Machine { name, ip: entry.from_ip, mac: entry.from_mac, l2: entry.session.l2, ..blank };
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
                if row.l2 != 0 {
                    draw!(" {}  {:<24} {:<15} {}  {} ({})\x1b[K", index + 1, row.name.as_str(), "layer 2", MacText(row.mac), state, if row.l2 == 2 { "AVTP" } else { "raw" });
                } else {
                    draw!(" {}  {:<24} {}.{}.{}.{} {}  {}\x1b[K", index + 1, row.name.as_str(), row.ip[0], row.ip[1], row.ip[2], row.ip[3], MacText(row.mac), state);
                }
            }
            if count == 0 {
                at(4, 1);
                draw!(" No other machine heard yet. Other machines show up here while their panel is open or while they stream");
                at(5, 1);
                draw!(" on the same kind of network (RTP, or raw Ethernet / AVTP on the same switch). It updates by itself.");
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
