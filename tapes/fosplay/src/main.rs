//! fosplay: the media player. `fosplay ls HOST SHARE [DIR]` lists a directory of a guest-accessible SMB share,
//! `fosplay cat HOST SHARE FILE` reads a file to the end and reports its size, speed and a checksum (FNV-1a 32), and
//! `fosplay [device=N|loopN] play HOST SHARE FILE` decodes a FLAC file while it streams in and plays it. All of them use
//! `crates/smb` over the shared TCP client (`tape_ip::tcp`), on the interface chosen with `nic use N` and an address from
//! DHCP. HOST is an IPv4 address or a host name.
//!
//! The player is hard-float: FLAC decoding is integer (`audio_decode::flac`), MP3 is float (`audio_decode::mp3`), the conversion to the device's format and the
//! resampling to a rate the device offers (cubic, `audio_decode::resample`) are `f32`. Nothing is printed between the first
//! and the last sample: a console line costs ~10 ms on real hardware.
#![no_std]
#![no_main]

mod ui;

use outfx::{Bass, BassBoost, Options, Upmix};
use ui::{Action, Ui};
use audio_decode::{Decode, Source, SourceError, flac, mp3, opus, resample::Resampler, wav};
use smb::{Client, Error, Handle, Transport};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{AudioInfo, AudioStats, KEY_ABORT, KEY_BASE, KEY_NONE, Line, TapeApi, println};

const SMB_PORT: u16 = 445;
/// How long the server may stay silent before a request counts as lost.
const TIMEOUT_MICROS: u64 = 5_000_000;

struct Link {
    socket: Socket<16384>,
    api: Option<&'static TapeApi>,
}

impl Transport for Link {
    fn send(&mut self, data: &[u8]) -> Result<(), Error> {
        let api = self.api.ok_or(Error::Transport)?;
        self.socket.send(api, data, TIMEOUT_MICROS).map_err(|_| Error::Transport)
    }

    fn recv_exact(&mut self, out: &mut [u8]) -> Result<(), Error> {
        let api = self.api.ok_or(Error::Transport)?;
        self.socket.recv_exact(api, out, TIMEOUT_MICROS).map_err(|_| Error::Transport)
    }
}

/// 64 KiB reads plus the headers; kept in a static because of its size.
static mut CLIENT: Client<Link, { 64 * 1024 + 1024 }> = Client::new(Link { socket: Socket::new(), api: None });

/// The file being played: on the share (read through the static `CLIENT`; the decoder owns this value, so it cannot
/// borrow the client) or a FOSFS file read into memory whole.
struct MediaSource {
    handle: Option<Handle>,
    memory: &'static [u8],
}

impl Source for MediaSource {
    fn size(&self) -> u64 {
        self.handle.map_or(self.memory.len() as u64, |handle| handle.size)
    }

    fn read_at(&mut self, offset: u64, out: &mut [u8]) -> Result<usize, SourceError> {
        let Some(handle) = self.handle else { return self.memory.read_at(offset, out) };
        #[allow(static_mut_refs)]
        let client = unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) };
        client.read(&handle, offset, out).map_err(|_| SourceError)
    }
}

static mut FLAC: flac::Decoder<MediaSource> = flac::Decoder::new(MediaSource { handle: None, memory: &[] });
static mut MP3: mp3::Decoder<MediaSource> = mp3::Decoder::new(MediaSource { handle: None, memory: &[] });
static mut OPUS: opus::Decoder<MediaSource, OpusCodec> = opus::Decoder::new(MediaSource { handle: None, memory: &[] }, OpusCodec { codec: None });
static mut WAV: wav::Decoder<MediaSource> = wav::Decoder::new(MediaSource { handle: None, memory: &[] });

/// Output rate forced with `rate=N`; 0 means the file's own rate when the device offers it.
static mut FORCED_RATE: u32 = 0;
/// The output effects asked for (`bass=DB`...).
static mut EFFECTS: Options = Options::OFF;
/// The 2 to 5.1 upmix (a 64 KiB delay line, so it lives here); `Output::upmix` says whether it is in use.
static mut UPMIX: Upmix = Upmix::new();
static mut RESAMPLER: Resampler = Resampler::new();
/// The touch screen (`gfx=off` or no pixel screen: none) and the number of the screen it takes (`screen=N`).
static mut UI: Option<Ui> = None;
static mut GFX: bool = true;
static mut UI_SCREEN: u32 = 0;
/// The volume slider, 0..1, kept across devices restarts.
static mut VOLUME: f32 = 1.0;

/// libopus for the Ogg Opus reader in `audio-decode`.
struct OpusCodec {
    codec: Option<opus_codec::Decoder>,
}

impl opus::PacketDecoder for OpusCodec {
    fn start(&mut self, channels: u32) -> bool {
        self.codec = None;
        self.codec = opus_codec::Decoder::new(48_000, channels as i32).ok();
        self.codec.is_some()
    }

    fn decode(&mut self, packet: &[u8], out: &mut [f32], max_frames: usize) -> Option<usize> {
        self.codec.as_mut()?.decode_float(packet, out, max_frames).ok()
    }

    fn reset(&mut self) {
        if let Some(codec) = self.codec.as_mut() {
            codec.reset();
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Flac,
    Mp3,
    Wav,
    Opus,
}

/// The decoder for a file name, by its extension (FLAC when it has none we know).
fn kind_of(path: &str) -> Kind {
    if has_extension(path, ".mp3") {
        Kind::Mp3
    } else if has_extension(path, ".wav") {
        Kind::Wav
    } else if has_extension(path, ".opus") {
        Kind::Opus
    } else {
        Kind::Flac
    }
}

/// The source of the decoder that handles `path` (chosen by its extension).
fn media_source(path: &str) -> &'static mut MediaSource {
    #[allow(static_mut_refs)]
    unsafe {
        match kind_of(path) {
            Kind::Mp3 => (*core::ptr::addr_of_mut!(MP3)).source(),
            Kind::Wav => (*core::ptr::addr_of_mut!(WAV)).source(),
            Kind::Opus => (*core::ptr::addr_of_mut!(OPUS)).source(),
            Kind::Flac => (*core::ptr::addr_of_mut!(FLAC)).source(),
        }
    }
}

fn decoder_for(path: &str) -> &'static mut dyn Decode {
    #[allow(static_mut_refs)]
    unsafe {
        match kind_of(path) {
            Kind::Mp3 => &mut *core::ptr::addr_of_mut!(MP3),
            Kind::Wav => &mut *core::ptr::addr_of_mut!(WAV),
            Kind::Opus => &mut *core::ptr::addr_of_mut!(OPUS),
            Kind::Flac => &mut *core::ptr::addr_of_mut!(FLAC),
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(rest) => rest,
        Err(reason) => return println!("fosplay: {reason}."),
    };
    let mut arguments = arguments;
    // Options come before the command: rate=HZ bass=DB bassfreq=HZ bassq=Q.
    while let Some((word, after)) = arguments.split_once(char::is_whitespace).or(Some((arguments, ""))) {
        let Some((key, value)) = word.split_once('=') else { break };
        if let Err(reason) = set_option(key, value) {
            return println!("fosplay: {reason}.");
        }
        arguments = after.trim_start();
    }
    let mut words = arguments.split_whitespace();
    let command = words.next().unwrap_or("");
    if command == "play" {
        return play_command(api, words);
    }
    if command == "browse" {
        return browse_command(api, words);
    }
    let rest: [&str; 3] = core::array::from_fn(|_| words.next().unwrap_or(""));
    let [host, share, path] = rest;
    if host.is_empty() || share.is_empty() || !matches!(command, "ls" | "cat") || (command == "cat" && path.is_empty()) {
        return usage();
    }
    #[allow(static_mut_refs)]
    let client = unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) };
    let Some(()) = open(api, client, host, share) else { return };
    match command {
        "ls" => list(client, path),
        _ => cat(api, client, path),
    }
    client.transport().socket.close(api);
}

/// One `key=value` option; the error says what was wrong.
fn set_option(key: &str, value: &str) -> Result<(), &'static str> {
    #[allow(static_mut_refs)]
    unsafe {
        match key {
            "rate" => match value.parse::<u32>() {
                Ok(rate) if (8000..=384_000).contains(&rate) => FORCED_RATE = rate,
                _ => return Err("rate= needs a sample rate such as 48000"),
            },
            "gfx" => match value {
                "on" => GFX = true,
                "off" => GFX = false,
                _ => return Err("gfx= takes on or off"),
            },
            "screen" => match value.parse::<u32>() {
                Ok(number) => UI_SCREEN = number,
                _ => return Err("screen= needs a screen number"),
            },
            _ => {
                if !EFFECTS.set(key, value, tape_rt::parse_f32)? {
                    return Err("unknown option (see `fosplay help`)");
                }
            }
        }
    }
    Ok(())
}

fn usage() {
    println!("usage: fosplay ls HOST SHARE [DIR] | cat HOST SHARE FILE");
    println!("       fosplay [device=N|loopN] [OPTIONS] play //HOST/SHARE[/FILE|DIR]");
    println!("       fosplay [device=N|loopN] [OPTIONS] play NAME...");
    println!("       fosplay [device=N|loopN] [OPTIONS] browse [//HOST/SHARE[/DIR]]");
}

type Player = Client<Link, { 64 * 1024 + 1024 }>;

/// Address by DHCP, the host's address (a dotted quad or a name asked of the DHCP-named DNS server, else the gateway), TCP
/// to port 445, SMB negotiate, guest session, tree connect.
fn open(api: &'static TapeApi, client: &mut Player, host: &str, share: &str) -> Option<()> {
    let nic = tape_rt::active_nic();
    if nic >= (api.nic_count)() as usize {
        println!("fosplay: no network interface.");
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let Some((config, _, dns_server)) = net::dhcp_with_servers(api, nic, MacAddress(mac)) else {
        println!("fosplay: no DHCP lease on NIC {nic}.");
        return None;
    };
    let server = dns_server.unwrap_or(config.gateway);
    let address = match tape_ip::dns::lookup(api, nic, &config, &mut ArpCache::<8>::new(), server, tape_net::dns::PORT, host) {
        Ok(address) => address,
        Err(why) => {
            println!("fosplay: {host}: {why}.");
            return None;
        }
    };
    println!("fosplay: {:?} -> {host} ({address:?}):{SMB_PORT}", config.ip);
    let link = client.transport();
    link.api = Some(api);
    if let Err(why) = link.socket.connect(api, nic, &config, address, SMB_PORT, 3_000_000) {
        println!("fosplay: {why}.");
        return None;
    }
    if let Err(error) = client.connect(host, share) {
        println!("fosplay: share {share}: {}.", error.describe());
        return None;
    }
    Some(())
}

fn list(client: &mut Player, path: &str) {
    let mut entries = 0u32;
    let result = client.list(path, |entry| {
        let mut name = [0u8; 120];
        let length = entry.name_utf8(&mut name);
        let name = core::str::from_utf8(&name[..length]).unwrap_or("?");
        if entry.is_dir() {
            println!("  <dir>        {name}");
        } else {
            println!("  {:>12} {name}", entry.size);
        }
        entries += 1;
    });
    match result {
        Ok(()) => println!("fosplay: {entries} entries."),
        Err(error) => println!("fosplay: {path}: {}.", error.describe()),
    }
}

fn cat(api: &TapeApi, client: &mut Player, path: &str) {
    let file = match client.open(path, false) {
        Ok(file) => file,
        Err(error) => return println!("fosplay: {path}: {}.", error.describe()),
    };
    println!("fosplay: {path}: {} bytes.", file.size);
    let mut buffer = [0u8; 65536];
    let (mut offset, mut hash) = (0u64, 0x811c_9dc5u32);
    let started = tape_rt::micros();
    while offset < file.size {
        if (api.should_abort)() != 0 {
            println!("fosplay: stopped.");
            break;
        }
        match client.read(&file, offset, &mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                for byte in &buffer[..count] {
                    hash = (hash ^ *byte as u32).wrapping_mul(0x0100_0193);
                }
                offset += count as u64;
            }
            Err(error) => {
                println!("fosplay: read at {offset}: {}.", error.describe());
                break;
            }
        }
    }
    let micros = (tape_rt::micros() - started).max(1);
    let _ = client.close(&file);
    println!("fosplay: read {offset} bytes in {} ms, {} kbit/s, fnv1a {hash:08x}.", micros / 1000, offset * 8 * 1000 / micros);
    if offset == file.size {
        println!("fosplay: cat OK.");
    }
}

/// Sends the stereo frames it is given to the audio device in its own format, blocking while the ring is full.
/// How far `f` and `b` jump.
const SKIP_SECONDS: i32 = 10;

/// What the listener asked for with a key, or how a track ended.
#[derive(Clone, Copy, PartialEq)]
enum Outcome {
    Done,
    Next,
    Previous,
    Stop,
    Failed,
    /// Jump by this many seconds (handled inside the track, never returned from it).
    Skip(i32),
    /// Seek to this permille of the track (likewise handled inside it).
    SeekTo(u32),
    /// Play this playlist entry.
    Jump(usize),
}

/// The device end of playback: converts frames to the device's format and writes them, watching the keys between
/// writes (`n` next, `p` previous, space pause, Esc or `q` stop).
struct Output {
    api: &'static TapeApi,
    format: audio_sample::Format,
    channels: usize,
    slot: usize,
    staging: [u8; 8192],
    staged: usize,
    paused: bool,
    command: Option<Outcome>,
    /// The bass boost, when asked for.
    bass: Option<Bass>,
    /// `UPMIX` feeds the first six channels (L R C LFE Ls Rs); the device has at least six.
    upmix: bool,
    /// The device has at least six outputs, so the upmix can be switched on.
    wide: bool,
    rate: u32,
    gain: f32,
    /// The largest left and right sample since the screen last looked.
    peak: [f32; 2],
    /// How far the decoder has got in the track, seconds (what is heard is `queued` earlier).
    decoded: f32,
    /// Ring space when it holds nothing.
    empty: usize,
}

/// A line under the title on the screen (what went wrong with a track); the next track clears it.
fn show_message(message: core::fmt::Arguments) {
    use core::fmt::Write;
    #[allow(static_mut_refs)]
    if let Some(ui) = unsafe { (*core::ptr::addr_of_mut!(UI)).as_mut() } {
        ui.view.message.clear();
        let _ = ui.view.message.write_fmt(message);
    }
}

/// The gain of the volume slider: `ui::VOLUME_RANGE_DB` below full scale at the bottom, silent at its very end.
fn volume_gain(slider: f32) -> f32 {
    if slider < 0.01 { 0.0 } else { dsp::level::db_amplitude((slider - 1.0) * ui::VOLUME_RANGE_DB) }
}

impl Output {
    fn frame(&mut self, mut frame: [f32; 2]) {
        if let Some(bass) = self.bass.as_mut() {
            frame = bass.process(frame);
        }
        frame = [frame[0] * self.gain, frame[1] * self.gain];
        self.peak = [self.peak[0].max(frame[0].abs()), self.peak[1].max(frame[1].abs())];
        if self.staged + self.channels * self.slot > self.staging.len() {
            self.flush();
        }
        let wide = if self.upmix {
            #[allow(static_mut_refs)]
            Some(unsafe { (*core::ptr::addr_of_mut!(UPMIX)).process(frame) })
        } else {
            None
        };
        for channel in 0..self.channels {
            let value = match wide {
                Some(six) => if channel < 6 { six[channel] } else { 0.0 },
                None => if channel < 2 { frame[channel] } else { 0.0 },
            };
            self.format.write(value.clamp(-1.0, 1.0), &mut self.staging[self.staged..self.staged + self.slot]);
            self.staged += self.slot;
        }
    }

    /// Clears the effects' history (after a seek).
    fn reset_effects(&mut self) {
        if let Some(bass) = self.bass.as_mut() {
            bass.reset();
        }
        if self.upmix {
            #[allow(static_mut_refs)]
            unsafe { (*core::ptr::addr_of_mut!(UPMIX)).reset() };
        }
    }

    /// Carries out what a finger asked for.
    fn apply(&mut self, action: Action) {
        #[allow(static_mut_refs)]
        let effects = unsafe { &mut *core::ptr::addr_of_mut!(EFFECTS) };
        match action {
            Action::Previous => self.command = Some(Outcome::Previous),
            Action::Next => self.command = Some(Outcome::Next),
            Action::Back => self.command = Some(Outcome::Skip(-SKIP_SECONDS)),
            Action::Forward => self.command = Some(Outcome::Skip(SKIP_SECONDS)),
            Action::Stop => self.command = Some(Outcome::Stop),
            Action::PlayPause => self.paused = !self.paused,
            Action::Seek(permille) => self.command = Some(Outcome::SeekTo(permille)),
            Action::Jump(index) => self.command = Some(Outcome::Jump(index)),
            Action::Volume(slider) => {
                unsafe { VOLUME = slider };
                self.gain = volume_gain(slider);
            }
            Action::Bass(db) => {
                let db = db.clamp(0.0, 12.0);
                effects.bass = BassBoost { gain_db: db, ..effects.bass };
                match (self.bass.as_mut(), db > 0.0) {
                    (Some(bass), true) => bass.set(effects.bass),
                    _ => self.bass = Bass::new(self.rate, effects.bass),
                }
            }
            Action::Width(width) => {
                effects.surround.width = width;
                self.retune();
            }
            Action::Upmix if self.wide => {
                self.upmix = !self.upmix;
                effects.upmix = self.upmix;
                #[allow(static_mut_refs)]
                unsafe { (*core::ptr::addr_of_mut!(UPMIX)).configure(self.rate, effects.surround) };
            }
            Action::Center => {
                effects.surround.center = !effects.surround.center;
                self.retune();
            }
            Action::Upmix | Action::Open(_) | Action::Parent | Action::PlayAll | Action::Quit => {}
        }
    }

    /// Hands the upmix its settings again without clearing its delay line.
    fn retune(&mut self) {
        if self.upmix {
            #[allow(static_mut_refs)]
            unsafe { (*core::ptr::addr_of_mut!(UPMIX)).tune(self.rate, EFFECTS.surround) };
        }
    }

    /// Fingers and the picture: runs often, draws at most every 50 ms and only when something changed.
    fn service_ui(&mut self) {
        #[allow(static_mut_refs)]
        let Some(ui) = (unsafe { (*core::ptr::addr_of_mut!(UI)).as_mut() }) else { return };
        while let Some(action) = ui.next_action() {
            self.apply(action);
        }
        if !ui.due() {
            return;
        }
        let mut info = AudioInfo::default();
        (self.api.audio_info)(&mut info);
        let queued = (self.empty.saturating_sub(info.playback_space as usize) / (self.channels * self.slot).max(1)) as f32 / self.rate.max(1) as f32;
        let effects = unsafe { EFFECTS };
        let view = &mut ui.view;
        view.seconds = (self.decoded - queued).max(0.0);
        view.paused = self.paused;
        view.level = [view.level[0] * 0.8, view.level[1] * 0.8];
        view.level = [view.level[0].max(self.peak[0]), view.level[1].max(self.peak[1])];
        self.peak = [0.0; 2];
        view.volume = unsafe { VOLUME };
        view.bass_db = effects.bass.gain_db;
        view.width = effects.surround.width;
        view.center = effects.surround.center;
        view.upmix = self.upmix;
        view.upmix_ok = self.wide;
        #[allow(static_mut_refs)]
        let list = unsafe { &*core::ptr::addr_of!(PLAYLIST) };
        ui.draw(|index| {
            let name = list.get(index);
            let name = name.rsplit('/').next().unwrap_or(name);
            name.rsplit_once('.').map_or(name, |(stem, _)| stem)
        });
    }

    /// Reads the keys; while paused it waits here, so nothing is decoded or written.
    fn poll(&mut self) {
        loop {
            self.service_ui();
            if self.command.is_some() {
                return;
            }
            let key = (self.api.read_key)();
            let command = match key {
                KEY_NONE if self.paused => {
                    tape_rt::idle();
                    continue;
                }
                KEY_NONE => return,
                KEY_ABORT => Some(Outcome::Stop),
                _ => match (key - KEY_BASE) as u8 {
                    b'n' => Some(Outcome::Next),
                    b'p' => Some(Outcome::Previous),
                    b'q' => Some(Outcome::Stop),
                    b'f' => Some(Outcome::Skip(SKIP_SECONDS)),
                    b'b' => Some(Outcome::Skip(-SKIP_SECONDS)),
                    b' ' => {
                        self.paused = !self.paused;
                        if self.paused {
                            println!("fosplay: paused (space resumes).");
                        }
                        None
                    }
                    _ => None,
                },
            };
            if command.is_some() {
                self.command = command;
                return;
            }
        }
    }

    fn flush(&mut self) {
        let mut sent = 0;
        while sent < self.staged && self.command.is_none() {
            let accepted = (self.api.audio_write)(self.staging[sent..self.staged].as_ptr(), self.staged - sent);
            sent += accepted;
            self.poll();
            if accepted == 0 && self.command.is_none() {
                tape_rt::idle();
            }
        }
        self.staged = 0;
    }
}

/// `[3/12] ` before a track's name when there is more than one.
struct Position(usize, usize);

impl core::fmt::Display for Position {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        if self.1 > 1 { write!(f, "[{}/{}] ", self.0, self.1) } else { Ok(()) }
    }
}

/// The names of the tracks, `dir/name` for a share, as given for FOSFS files.
struct Playlist {
    names: [u8; 24576],
    used: usize,
    items: [(u16, u8); 192],
    count: usize,
}

static mut PLAYLIST: Playlist = Playlist::new();
/// What the browser shows: the current folder's sub-folders (names end in `/`) and audio files.
static mut ENTRIES: Playlist = Playlist::new();

impl Playlist {
    const fn new() -> Self {
        Playlist { names: [0; 24576], used: 0, items: [(0, 0); 192], count: 0 }
    }

    fn clear(&mut self) {
        self.used = 0;
        self.count = 0;
    }

    fn add(&mut self, directory: &str, name: &str) -> bool {
        let separator = !directory.is_empty() && !directory.ends_with(['/', '\\']);
        let length = directory.len() + separator as usize + name.len();
        if self.count == self.items.len() || self.used + length > self.names.len() || length > 255 {
            return false;
        }
        let at = self.used;
        self.names[at..at + directory.len()].copy_from_slice(directory.as_bytes());
        let mut end = at + directory.len();
        if separator {
            self.names[end] = b'/';
            end += 1;
        }
        self.names[end..end + name.len()].copy_from_slice(name.as_bytes());
        self.items[self.count] = (at as u16, length as u8);
        self.count += 1;
        self.used += length;
        true
    }

    fn get(&self, index: usize) -> &str {
        let (at, length) = self.items[index];
        core::str::from_utf8(&self.names[at as usize..at as usize + length as usize]).unwrap_or("?")
    }

    /// By name, byte order: files numbered `01_...` play in order.
    fn sort(&mut self) {
        self.sort_by(|a, b| a.as_bytes() < b.as_bytes());
    }

    /// Insertion sort (stable); `before(a, b)` says `a` goes ahead of `b`.
    fn sort_by(&mut self, before: impl Fn(&str, &str) -> bool) {
        for i in 1..self.count {
            let mut j = i;
            while j > 0 && before(self.get(j), self.get(j - 1)) {
                self.items.swap(j - 1, j);
                j -= 1;
            }
        }
    }
}

fn has_extension(path: &str, extension: &str) -> bool {
    let (path, extension) = (path.as_bytes(), extension.as_bytes());
    path.len() > extension.len() && path[path.len() - extension.len()..].eq_ignore_ascii_case(extension)
}

fn is_audio(path: &str) -> bool {
    has_extension(path, ".flac") || has_extension(path, ".mp3") || has_extension(path, ".wav") || has_extension(path, ".opus")
}

/// `fosplay play HOST SHARE [FILE|DIR]` (a directory plays its .flac, .mp3, .wav and .opus files in name order) or
/// `fosplay play NAME...` (FOSFS files).
fn play_command<'a>(api: &'static TapeApi, mut words: impl Iterator<Item = &'a str>) {
    let first = words.next().unwrap_or("");
    if first.is_empty() {
        return usage();
    }
    #[allow(static_mut_refs)]
    let list = unsafe { &mut *core::ptr::addr_of_mut!(PLAYLIST) };
    let Some((first, share, path)) = parse_share(first, &mut words) else {
        for name in core::iter::once(first).chain(words) {
            if !list.add("", name) {
                return println!("fosplay: too many files.");
            }
        }
        return run(api, None, 0);
    };
    if first.is_empty() || share.is_empty() {
        return usage();
    }
    #[allow(static_mut_refs)]
    let client = unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) };
    let Some(()) = open(api, client, first, share) else { return };
    if is_audio(path) {
        list.add("", path);
    } else {
        let result = client.list(path, |entry| {
            if entry.is_dir() {
                return;
            }
            let mut name = [0u8; 120];
            let length = entry.name_utf8(&mut name);
            let name = core::str::from_utf8(&name[..length]).unwrap_or("");
            if is_audio(name) && !name.starts_with('.') && !list.add(path, name) {
                println!("fosplay: the playlist is full; the rest is left out.");
            }
        });
        if let Err(error) = result {
            println!("fosplay: {path}: {}.", error.describe());
        }
        list.sort();
    }
    if list.count == 0 {
        println!("fosplay: no .flac, .mp3, .wav or .opus files found.");
    } else {
        run(api, Some(client), 0);
    }
    client.transport().socket.close(api);
}

/// `//HOST/SHARE[/PATH]` or `HOST SHARE [PATH]` with a dotted-quad HOST name an SMB share; anything else is FOSFS.
fn parse_share<'a>(first: &'a str, words: &mut impl Iterator<Item = &'a str>) -> Option<(&'a str, &'a str, &'a str)> {
    if let Some(unc) = first.strip_prefix("//") {
        let (host, rest) = unc.split_once('/').unwrap_or((unc, ""));
        let (share, path) = rest.split_once('/').unwrap_or((rest, ""));
        Some((host, share, path.trim_end_matches('/')))
    } else if net::parse_ip(first).is_some() {
        Some((first, words.next().unwrap_or(""), words.next().unwrap_or("")))
    } else {
        None
    }
}

/// The device across tracks: it stays open while the next track wants the same output rate, so an album at one rate
/// plays without restarting it.
struct Session {
    api: &'static TapeApi,
    output: Option<Output>,
    /// What the first choice of rate was when the device was started (a track asks for the same, it keeps running).
    wanted: u32,
    out_rate: u32,
    /// Ring space when it holds nothing.
    empty: usize,
    started: u64,
    tracks: u32,
}

impl Session {
    /// Waits for the queued audio to play out (when `drain`), reports, and stops the device.
    fn close(&mut self, drain: bool, report: bool) {
        let Some(mut output) = self.output.take() else { return };
        let mut info = AudioInfo::default();
        if drain && output.command.is_none() {
            output.flush();
            let mut waited = 0;
            while output.command.is_none() && waited < 120_000 {
                (self.api.audio_info)(&mut info);
                if info.playback_space as usize >= self.empty {
                    break;
                }
                output.poll();
                tape_rt::idle();
                waited += 1;
            }
        }
        let mut stats = AudioStats::default();
        (self.api.audio_stats)(&mut stats);
        (self.api.audio_stop)();
        let seconds = (tape_rt::micros() - self.started) / 1_000_000;
        if report {
            println!("fosplay: {} track(s), {seconds} s, {} packets, {} underrun(s).", self.tracks, stats.playback_packets, stats.playback_underruns);
        }
    }
}

/// Plays the playlist from entry `start`. Takes the touch screen unless the browser already holds it.
fn run(api: &'static TapeApi, mut client: Option<&mut Player>, start: usize) {
    #[allow(static_mut_refs)]
    let list = unsafe { &*core::ptr::addr_of!(PLAYLIST) };
    let mut session = Session { api, output: None, wanted: 0, out_rate: 0, empty: 0, started: 0, tracks: 0 };
    let total = list.count;
    let mut index = start;
    let mut stopped = false;
    #[allow(static_mut_refs)]
    let own_ui = unsafe { (*core::ptr::addr_of!(UI)).is_none() };
    #[allow(static_mut_refs)]
    unsafe {
        if own_ui {
            *core::ptr::addr_of_mut!(UI) = if GFX { Ui::open(api, UI_SCREEN) } else { None };
        }
    }
    while index < total {
        let path = list.get(index);
        let outcome = match client.as_deref_mut() {
            Some(client) => match open_remote(client, path) {
                Some(file) => {
                    media_source(path).handle = Some(file);
                    let outcome = play_track(&mut session, path, index, total);
                    let _ = client.close(&file);
                    outcome
                }
                None => Outcome::Failed,
            },
            None => play_local(&mut session, path, index, total),
        };
        match outcome {
            Outcome::Done | Outcome::Next | Outcome::Failed | Outcome::Skip(_) | Outcome::SeekTo(_) => index += 1,
            Outcome::Previous => index = index.saturating_sub(1),
            Outcome::Jump(target) => index = target.min(total.saturating_sub(1)),
            Outcome::Stop => {
                stopped = true;
                break;
            }
        }
    }
    session.close(!stopped, true);
    #[allow(static_mut_refs)]
    if own_ui {
        if let Some(ui) = unsafe { (*core::ptr::addr_of_mut!(UI)).take() } {
            ui.close();
        }
    }
}

/// `fosplay browse //HOST/SHARE[/DIR]` (or `HOST SHARE [DIR]`) or `fosplay browse [VOLUME]` for FOSFS and `diskN` files:
/// the touch screen lists folders and audio files; a tap on a folder enters it, on a file plays the folder from there.
fn browse_command<'a>(api: &'static TapeApi, mut words: impl Iterator<Item = &'a str>) {
    if !unsafe { GFX } {
        return println!("fosplay: browse needs the touch screen (gfx=off is set).");
    }
    let first = words.next().unwrap_or("");
    let target = parse_share(first, &mut words);
    #[allow(static_mut_refs)]
    let client = unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) };
    if let Some((host, share, _)) = target {
        if host.is_empty() || share.is_empty() || open(api, client, host, share).is_none() {
            return;
        }
    }
    match Ui::open(api, unsafe { UI_SCREEN }) {
        Some(ui) => unsafe { *core::ptr::addr_of_mut!(UI) = Some(ui) },
        None => println!("fosplay: browse needs a pixel screen."),
    }
    #[allow(static_mut_refs)]
    if unsafe { (*core::ptr::addr_of!(UI)).is_some() } {
        let remote = target.is_some();
        browse(api, remote.then_some(&mut *client), target.map_or(first, |(_, _, path)| path), target.map(|(host, share, _)| (host, share)));
        #[allow(static_mut_refs)]
        if let Some(ui) = unsafe { (*core::ptr::addr_of_mut!(UI)).take() } {
            ui.close();
        }
    }
    if target.is_some() {
        client.transport().socket.close(api);
    }
}

fn ui() -> Option<&'static mut Ui> {
    #[allow(static_mut_refs)]
    unsafe { (*core::ptr::addr_of_mut!(UI)).as_mut() }
}

/// Case-blind name order with folders first and `../` before everything.
fn browse_order(a: &str, b: &str) -> bool {
    let key = |name: &str| (name != "../", !name.ends_with('/'));
    match key(a).cmp(&key(b)) {
        core::cmp::Ordering::Equal => a.bytes().map(|byte| byte.to_ascii_lowercase()).cmp(b.bytes().map(|byte| byte.to_ascii_lowercase())) == core::cmp::Ordering::Less,
        order => order == core::cmp::Ordering::Less,
    }
}

/// Fills `ENTRIES` with what `path` holds; an error is shown on the screen and leaves the list empty.
fn load_entries(api: &TapeApi, client: Option<&mut Player>, path: &str) {
    #[allow(static_mut_refs)]
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(ENTRIES) };
    entries.clear();
    let mut full = false;
    let mut add = |entries: &mut Playlist, name: &str, folder: bool| {
        if name.starts_with('.') || (!folder && !is_audio(name)) {
            return;
        }
        let mut label = Line::<132>::new();
        let _ = write!(label, "{name}{}", if folder { "/" } else { "" });
        full |= !entries.add("", label.as_str());
    };
    use core::fmt::Write;
    match client {
        Some(client) => {
            if !path.is_empty() {
                entries.add("", "../");
            }
            if let Err(error) = client.list(path, |entry| {
                let mut name = [0u8; 120];
                let length = entry.name_utf8(&mut name);
                add(entries, core::str::from_utf8(&name[..length]).unwrap_or(""), entry.is_dir());
            }) {
                show_message(format_args!("{path}: {}", error.describe()));
            }
        }
        None => {
            for index in 0.. {
                match tape_rt::list_file(api, path, index) {
                    Ok(Some(info)) => {
                        let name = core::str::from_utf8(&info.name[..info.name_length as usize]).unwrap_or("");
                        add(entries, name, info.flags & tape_rt::FILE_DIRECTORY != 0);
                    }
                    Ok(None) => break,
                    Err(_) => {
                        show_message(format_args!("cannot list {}", if path.is_empty() { "the boot volume" } else { path }));
                        break;
                    }
                }
            }
        }
    }
    entries.sort_by(browse_order);
    if full {
        show_message(format_args!("the folder has more entries than fit; the rest is left out"));
    }
}

/// Puts the audio files of `ENTRIES` (in `path`) in the playlist and returns the index of `first` in it (0 when absent).
fn playlist_from_entries(path: &str, first: &str) -> usize {
    #[allow(static_mut_refs)]
    let (list, entries) = unsafe { (&mut *core::ptr::addr_of_mut!(PLAYLIST), &*core::ptr::addr_of!(ENTRIES)) };
    list.clear();
    let mut start = 0;
    for index in 0..entries.count {
        let name = entries.get(index);
        if !name.ends_with('/') && list.add(path, name) && name == first {
            start = list.count - 1;
        }
    }
    start
}

/// The folder above `path` (`a/b` -> `a`, `a` -> the share's root).
fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

fn browse(api: &'static TapeApi, mut client: Option<&mut Player>, start: &str, share: Option<(&str, &str)>) {
    use core::fmt::Write;
    let mut path = Line::<200>::new();
    let _ = write!(path, "{}", start.trim_matches('/'));
    let mut reload = true;
    let mut refresh = true;
    if let Some(ui) = ui() {
        ui.set_browse(true);
    }
    loop {
        let Some(ui) = ui() else { return };
        if reload {
            ui.view.message.clear();
            load_entries(api, client.as_deref_mut(), path.as_str());
            ui.reset_list();
            reload = false;
            refresh = true;
        }
        #[allow(static_mut_refs)]
        let entries = unsafe { &*core::ptr::addr_of!(ENTRIES) };
        if refresh {
            refresh = false;
            ui.view.count = entries.count;
            ui.view.title.clear();
            match share {
                Some((host, name)) => {
                    let _ = write!(ui.view.title, "//{host}/{name}/{}", path.as_str());
                }
                None => {
                    let _ = write!(ui.view.title, "{}", if path.is_empty() { "FOSFS" } else { path.as_str() });
                }
            }
            ui.view.detail.clear();
            let _ = write!(ui.view.detail, "{} entries", entries.count);
        }
        let mut action = ui.next_action();
        if action.is_none() {
            let key = (api.read_key)();
            action = match key {
                KEY_NONE => None,
                KEY_ABORT => Some(Action::Quit),
                _ => match (key - KEY_BASE) as u8 {
                    b'q' => Some(Action::Quit),
                    b'j' => {
                        ui.cursor_by(1);
                        None
                    }
                    b'k' => {
                        ui.cursor_by(-1);
                        None
                    }
                    b'\r' | b'\n' => (ui.view.cursor < entries.count).then_some(Action::Open(ui.view.cursor)),
                    0x08 | 0x7f => Some(Action::Parent),
                    b'a' => Some(Action::PlayAll),
                    _ => None,
                },
            };
        }
        let Some(action) = action else {
            if ui.due() {
                ui.draw(|index| match entries.get(index) {
                    "../" => ".. (up)",
                    name => name,
                });
            } else {
                tape_rt::idle();
            }
            continue;
        };
        let mut chosen = Line::<132>::new();
        let mut play_from = None;
        match action {
            Action::Quit => return,
            Action::Parent => {
                if client.is_some() && !path.is_empty() {
                    let parent = parent_of(path.as_str());
                    let mut up = Line::<200>::new();
                    let _ = write!(up, "{parent}");
                    path = up;
                    reload = true;
                }
            }
            Action::PlayAll => play_from = Some(""),
            Action::Open(index) if index < entries.count => {
                let _ = write!(chosen, "{}", entries.get(index));
                if chosen.as_str() == "../" {
                    let mut up = Line::<200>::new();
                    let _ = write!(up, "{}", parent_of(path.as_str()));
                    path = up;
                    reload = true;
                } else if let Some(folder) = chosen.as_str().strip_suffix('/') {
                    let mut down = Line::<200>::new();
                    let _ = write!(down, "{}{}{folder}", path.as_str(), if path.is_empty() || client.is_none() { "" } else { "/" });
                    path = down;
                    reload = true;
                } else {
                    play_from = Some(chosen.as_str());
                }
            }
            _ => {}
        }
        if let Some(first) = play_from {
            let start = playlist_from_entries(path.as_str(), first);
            #[allow(static_mut_refs)]
            if unsafe { (*core::ptr::addr_of!(PLAYLIST)).count } == 0 {
                show_message(format_args!("no audio files here"));
                continue;
            }
            ui.set_browse(false);
            run(api, client.as_deref_mut(), start);
            if let Some(ui) = self::ui() {
                ui.set_browse(true);
            }
            refresh = true;
        }
    }
}

fn open_remote(client: &mut Player, path: &str) -> Option<Handle> {
    match client.open(path, false) {
        Ok(file) => Some(file),
        Err(error) => {
            println!("fosplay: {path}: {}.", error.describe());
            None
        }
    }
}

/// A FOSFS file, read into memory whole (the tape ABI has no partial reads), then decoded from there.
fn play_local(session: &mut Session, name: &str, index: usize, total: usize) -> Outcome {
    let api = session.api;
    let mut capacity = 4 << 20;
    let (buffer, length) = loop {
        let buffer = (api.alloc)(capacity);
        if buffer == 0 {
            println!("fosplay: not enough memory.");
            return Outcome::Failed;
        }
        let result = (api.fs_read)(name.as_ptr(), name.len(), buffer as *mut u8, capacity);
        if result == tape_rt::FS_TOO_BIG && capacity < (256 << 20) {
            (api.free)(buffer, capacity);
            capacity *= 4;
            continue;
        }
        if result < 0 {
            (api.free)(buffer, capacity);
            println!("fosplay: cannot read {name} (code {result}).");
            return Outcome::Failed;
        }
        break (buffer, result as usize);
    };
    let source = media_source(name);
    source.handle = None;
    source.memory = unsafe { core::slice::from_raw_parts(buffer as *const u8, length) };
    let outcome = play_track(session, name, index, total);
    source.memory = &[];
    (api.free)(buffer, capacity);
    outcome
}

/// Plays the file behind the source `media_source(path)` was given.
fn play_track(session: &mut Session, path: &str, index: usize, total: usize) -> Outcome {
    let api = session.api;
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        println!("fosplay: no audio device with outputs (USB audio, HD Audio or a loopN).");
        return Outcome::Stop;
    }
    let decoder = decoder_for(path);
    if let Err(error) = decoder.open() {
        println!("fosplay: {path}: {}.", error.describe());
        show_message(format_args!("{path}: {}", error.describe()));
        return Outcome::Failed;
    }
    let rate = decoder.sample_rate();
    let offered = &info.rates[..info.rate_count as usize];
    let forced = unsafe { FORCED_RATE };
    // The file's own rate when the device has it, else 48 kHz, else whatever it offers; `rate=N` goes first. A rate the
    // device lists but then refuses falls through to the next candidate (the file is resampled to it).
    let mut candidates = [0u32; 18];
    let mut wanted = 0;
    let mut want = |rate: u32| {
        if rate != 0 && wanted < candidates.len() && !candidates[..wanted].contains(&rate) {
            candidates[wanted] = rate;
            wanted += 1;
        }
    };
    want(forced);
    if forced == 0 && (offered.is_empty() || offered.contains(&rate)) {
        want(rate);
    }
    if offered.contains(&48000) {
        want(48000);
    }
    offered.iter().for_each(|&r| want(r));
    let float = tape_rt::audio_float().1;
    if session.output.is_some() && session.wanted != candidates[0] {
        session.close(true, false);
    }
    if session.output.is_none() {
        let mut status = -1;
        for &candidate in &candidates[..wanted] {
            status = (api.audio_start)(candidate, 0, 1);
            if status == 0 {
                session.out_rate = candidate;
                break;
            }
        }
        if status != 0 {
            println!("fosplay: cannot start playback (code {status}); the device offers {offered:?}.");
            return Outcome::Stop;
        }
        session.wanted = candidates[0];
        (api.audio_info)(&mut info);
        session.empty = (info.playback_space as usize).saturating_sub(64).min((1 << 20) - 64);
        session.started = tape_rt::micros();
        session.tracks = 0;
        let effects = unsafe { EFFECTS };
        let upmix = effects.upmix && info.playback_channels >= 6;
        if effects.upmix && !upmix {
            println!("fosplay: upmix needs 6 playback channels; the device has {} (`audio modes`, `audio use playback N`): stereo.", info.playback_channels);
        }
        if upmix {
            #[allow(static_mut_refs)]
            unsafe { (*core::ptr::addr_of_mut!(UPMIX)).configure(session.out_rate, effects.surround) };
            println!("fosplay: upmix to 5.1 on {} channels (L R C LFE Ls Rs{}).", info.playback_channels, if effects.surround.center { "" } else { ", no centre" });
        }
        session.output = Some(Output {
            api,
            format: audio_sample::Format::new(info.playback_slot_bytes as usize, info.playback_bits as usize, float),
            channels: info.playback_channels as usize,
            slot: info.playback_slot_bytes as usize,
            staging: [0; 8192],
            staged: 0,
            paused: false,
            command: None,
            bass: Bass::new(session.out_rate, unsafe { EFFECTS }.bass),
            upmix,
            wide: info.playback_channels >= 6,
            rate: session.out_rate,
            gain: volume_gain(unsafe { VOLUME }),
            peak: [0.0; 2],
            decoded: 0.0,
            empty: session.empty,
        });
    }
    let out_rate = session.out_rate;
    let depth = decoder.format_name();
    let position = Position(index + 1, total);
    println!(
        "fosplay: {position}{path}: {depth}, {rate} Hz, {} channel(s), {} s; to {out_rate} Hz{}, {}{}-bit.",
        decoder.channels(), decoder.total_samples() / rate as u64, if out_rate == rate { "" } else { " (resampled)" },
        if float { "float " } else { "" }, info.playback_bits
    );
    let tags = decoder.tags();
    if !tags.title.is_empty() {
        println!(
            "fosplay: {position}{}{}{}{}{}",
            tags.title.as_str(),
            if tags.artist.is_empty() { "" } else { " - " }, tags.artist.as_str(),
            if tags.album.is_empty() { "" } else { " - " }, tags.album.as_str()
        );
    }
    session.tracks += 1;
    #[allow(static_mut_refs)]
    if let Some(ui) = unsafe { (*core::ptr::addr_of_mut!(UI)).as_mut() } {
        use core::fmt::Write;
        let view = &mut ui.view;
        view.title.clear();
        view.detail.clear();
        view.format.clear();
        view.message.clear();
        let base = path.rsplit('/').next().unwrap_or(path);
        let _ = write!(view.title, "{}", if tags.title.is_empty() { base } else { tags.title.as_str() });
        let _ = write!(view.detail, "{}{}{}", tags.artist.as_str(), if tags.artist.is_empty() || tags.album.is_empty() { "" } else { " - " }, tags.album.as_str());
        let _ = write!(view.format, "{depth} {rate} Hz, {} ch{}, {out_rate} Hz {}{}-bit", decoder.channels(),
                       if out_rate == rate { "" } else { " (resampled)" }, if float { "float " } else { "" }, info.playback_bits);
        view.index = index;
        view.count = total;
        view.total = decoder.total_samples() as f32 / rate as f32;
        view.seconds = 0.0;
    }
    let mut output = session.output.take().unwrap();
    output.decoded = 0.0;
    output.command = None;
    #[allow(static_mut_refs)]
    let resampler = unsafe { &mut *core::ptr::addr_of_mut!(RESAMPLER) };
    resampler.configure(rate, out_rate);
    let mut pcm = [0f32; 2 * 512];
    let mut failure = None;
    'blocks: loop {
        let count = match decoder.next_block() {
            Ok(Some(count)) => count,
            Ok(None) => break,
            Err(error) => {
                failure = Some(error.describe());
                break;
            }
        };
        let mut at = 0;
        while at < count {
            let step = (count - at).min(512);
            decoder.to_f32_stereo(at, step, &mut pcm);
            for frame in pcm[..2 * step].chunks_exact(2) {
                if out_rate == rate {
                    output.frame([frame[0], frame[1]]);
                } else {
                    resampler.push([frame[0], frame[1]], &mut |sample| output.frame(sample));
                }
            }
            if let Some(Outcome::Skip(_) | Outcome::SeekTo(_)) = output.command {
                let target = match output.command.take() {
                    Some(Outcome::Skip(seconds)) => (decoder.position() as i64 + seconds as i64 * rate as i64).max(0) as u64,
                    Some(Outcome::SeekTo(permille)) => decoder.total_samples() * permille as u64 / 1000,
                    _ => 0,
                };
                match decoder.seek(target) {
                    Ok(start) => {
                        // What is queued is from before the jump: drop what is staged, keep the device running.
                        output.staged = 0;
                        resampler.reset();
                        output.reset_effects();
                        output.decoded = start as f32 / rate as f32;
                        println!("fosplay: at {}:{:02} of {}:{:02}.", start / rate as u64 / 60, start / rate as u64 % 60,
                                 decoder.total_samples() / rate as u64 / 60, decoder.total_samples() / rate as u64 % 60);
                        continue 'blocks;
                    }
                    Err(error) => {
                        failure = Some(error.describe());
                        break 'blocks;
                    }
                }
            }
            if output.command.is_some() {
                break 'blocks;
            }
            at += step;
        }
        output.decoded = decoder.position() as f32 / rate as f32;
    }
    if output.command.is_none() && failure.is_none() && out_rate != rate {
        resampler.finish(&mut |sample| output.frame(sample));
    }
    if output.command.is_none() {
        output.flush();
    }
    let command = output.command;
    session.output = Some(output);
    if let Some(why) = failure {
        println!("fosplay: stopped: {why}.");
        show_message(format_args!("stopped: {why}"));
        return Outcome::Failed;
    }
    match command {
        None | Some(Outcome::Skip(_) | Outcome::SeekTo(_)) => Outcome::Done,
        // Next and previous cut the queued audio at once: stopping the device empties its ring.
        Some(outcome) => {
            if outcome != Outcome::Stop {
                session.close(false, false);
            }
            outcome
        }
    }
}

tape_rt::tape_help!(br#"fosplay: play audio from an SMB share (guest access).

  tape run fosplay ls HOST SHARE [DIR]
  tape run fosplay cat HOST SHARE FILE
  tape run fosplay [device=N|loopN] [OPTIONS] play //HOST/SHARE[/FILE|DIR]
  tape run fosplay [device=N|loopN] [OPTIONS] play IP SHARE [FILE|DIR]
  tape run fosplay [device=N|loopN] [OPTIONS] play NAME...     (FOSFS files)
  tape run fosplay [device=N|loopN] [OPTIONS] browse [//HOST/SHARE[/DIR]]

  A directory plays its .flac, .mp3, .wav and .opus files in name order.
  Keys: n next, p previous, space pause, f / b jump 10 s forward / back,
  Esc or q stop.
  With a pixel screen it also shows a touch panel: title, a progress bar
  (touch to seek), transport buttons, the playlist (touch to jump), a level
  meter, volume, bass boost and the 5.1 upmix switches. gfx=off hides it,
  screen=N picks the screen.
  browse shows the folders and audio files of a share (or, without one, of
  FOSFS; `browse diskN` a FAT volume) on that screen: touch a folder to open
  it, a file to play its folder from there, Play all, Up, Quit. Keys: j k
  move, Enter opens, Backspace up, a play all, q quits. Stop returns to the
  list.

  HOST is an IPv4 address or a name (asked of the DNS server DHCP names, else
  the gateway); our address comes from DHCP on the NIC chosen with
  `nic use N`. FLAC (mono or stereo, up to 24 bits), MP3 (any MPEG
  layer; no gapless trim), WAV (PCM 8-32 bit, float32) and Ogg Opus (.opus);
  the extension chooses. A rate the device does not offer is resampled
  (windowed sinc).

  OPTIONS (before the command):
    rate=HZ       force the output rate
    bass=DB       bass boost 0-12 dB (peaking filter, as in the phono tape);
                  the level drops by the same DB so a full-scale note cannot
                  clip
    bassfreq=HZ   its centre, 60-150 (default 100)
    bassq=Q       its width, 0.3 broad - 3 narrow (default 0.7)
    upmix=on      stereo to 5.1 on the device's first six channels, order
                  L R C LFE Ls Rs (pick a 6-channel mode: `audio use playback N`)
    center=off    leave the centre silent, fronts untouched (default on)
    width=0..1    surround level (0.7), lfe=HZ crossover 40-200 (100),
    delay=MS      surround delay 0-40 (15)
"#);
