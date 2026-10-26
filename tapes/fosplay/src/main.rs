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

use audio_decode::{Decode, Source, SourceError, flac, mp3, resample::Resampler, wav};
use smb::{Client, Error, Handle, Transport};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{ArpCache, Ipv4Address, MacAddress};
use tape_rt::{AudioInfo, AudioStats, KEY_ABORT, KEY_BASE, KEY_NONE, TapeApi, println};

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
static mut WAV: wav::Decoder<MediaSource> = wav::Decoder::new(MediaSource { handle: None, memory: &[] });

/// Output rate forced with `rate=N`; 0 means the file's own rate when the device offers it.
static mut FORCED_RATE: u32 = 0;
static mut RESAMPLER: Resampler = Resampler::new();

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Flac,
    Mp3,
    Wav,
}

/// The decoder for a file name, by its extension (FLAC when it has none we know).
fn kind_of(path: &str) -> Kind {
    if has_extension(path, ".mp3") {
        Kind::Mp3
    } else if has_extension(path, ".wav") {
        Kind::Wav
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
    if let Some(rest) = arguments.strip_prefix("rate=") {
        let (number, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
        match number.parse::<u32>() {
            Ok(rate) if (8000..=384_000).contains(&rate) => unsafe { FORCED_RATE = rate },
            _ => return println!("fosplay: rate= needs a sample rate such as 48000."),
        }
        arguments = after.trim_start();
    }
    let mut words = arguments.split_whitespace();
    let command = words.next().unwrap_or("");
    if command == "play" {
        return play_command(api, words);
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

fn usage() {
    println!("usage: fosplay ls HOST SHARE [DIR] | cat HOST SHARE FILE");
    println!("       fosplay [device=N|loopN] [rate=HZ] play //HOST/SHARE[/FILE|DIR]");
    println!("       fosplay [device=N|loopN] [rate=HZ] play NAME...");
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
}

impl Output {
    fn frame(&mut self, frame: [f32; 2]) {
        if self.staged + self.channels * self.slot > self.staging.len() {
            self.flush();
        }
        for channel in 0..self.channels {
            let value = if channel < 2 { frame[channel].clamp(-1.0, 1.0) } else { 0.0 };
            self.format.write(value, &mut self.staging[self.staged..self.staged + self.slot]);
            self.staged += self.slot;
        }
    }

    /// Reads the keys; while paused it waits here, so nothing is decoded or written.
    fn poll(&mut self) {
        loop {
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

static mut PLAYLIST: Playlist = Playlist { names: [0; 24576], used: 0, items: [(0, 0); 192], count: 0 };

impl Playlist {
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
        for i in 1..self.count {
            let mut j = i;
            while j > 0 && self.get(j - 1).as_bytes() > self.get(j).as_bytes() {
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
    has_extension(path, ".flac") || has_extension(path, ".mp3") || has_extension(path, ".wav")
}

/// `fosplay play HOST SHARE [FILE|DIR]` (a directory plays its .flac, .mp3 and .wav files in name order) or
/// `fosplay play NAME...` (FOSFS files).
fn play_command<'a>(api: &'static TapeApi, mut words: impl Iterator<Item = &'a str>) {
    let first = words.next().unwrap_or("");
    if first.is_empty() {
        return usage();
    }
    #[allow(static_mut_refs)]
    let list = unsafe { &mut *core::ptr::addr_of_mut!(PLAYLIST) };
    // `//HOST/SHARE[/PATH]` or `HOST SHARE [PATH]` with a dotted-quad HOST name an SMB share; anything else is FOSFS files.
    let (first, share, path) = if let Some(unc) = first.strip_prefix("//") {
        let (host, rest) = unc.split_once('/').unwrap_or((unc, ""));
        let (share, path) = rest.split_once('/').unwrap_or((rest, ""));
        (host, share, path.trim_end_matches('/'))
    } else if net::parse_ip(first).is_some() {
        (first, words.next().unwrap_or(""), words.next().unwrap_or(""))
    } else {
        for name in core::iter::once(first).chain(words) {
            if !list.add("", name) {
                return println!("fosplay: too many files.");
            }
        }
        return run(api, None);
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
        println!("fosplay: no .flac, .mp3 or .wav files found.");
    } else {
        run(api, Some(client));
    }
    client.transport().socket.close(api);
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

fn run(api: &'static TapeApi, mut client: Option<&mut Player>) {
    #[allow(static_mut_refs)]
    let list = unsafe { &*core::ptr::addr_of!(PLAYLIST) };
    let mut session = Session { api, output: None, wanted: 0, out_rate: 0, empty: 0, started: 0, tracks: 0 };
    let total = list.count;
    let mut index = 0;
    let mut stopped = false;
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
            Outcome::Done | Outcome::Next | Outcome::Failed | Outcome::Skip(_) => index += 1,
            Outcome::Previous => index = index.saturating_sub(1),
            Outcome::Stop => {
                stopped = true;
                break;
            }
        }
    }
    session.close(!stopped, true);
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
        session.output = Some(Output {
            api,
            format: audio_sample::Format::new(info.playback_slot_bytes as usize, info.playback_bits as usize, float),
            channels: info.playback_channels as usize,
            slot: info.playback_slot_bytes as usize,
            staging: [0; 8192],
            staged: 0,
            paused: false,
            command: None,
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
    let mut output = session.output.take().unwrap();
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
            if let Some(Outcome::Skip(seconds)) = output.command {
                output.command = None;
                let target = (decoder.position() as i64 + seconds as i64 * rate as i64).max(0) as u64;
                match decoder.seek(target) {
                    Ok(start) => {
                        // What is queued is from before the jump: drop what is staged, keep the device running.
                        output.staged = 0;
                        resampler.reset();
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
        return Outcome::Failed;
    }
    match command {
        None | Some(Outcome::Skip(_)) => Outcome::Done,
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
  tape run fosplay [device=N|loopN] [rate=HZ] play //HOST/SHARE[/FILE|DIR]
  tape run fosplay [device=N|loopN] [rate=HZ] play IP SHARE [FILE|DIR]
  tape run fosplay [device=N|loopN] [rate=HZ] play NAME...     (FOSFS files)

  A directory plays its .flac, .mp3 and .wav files in name order. Keys: n next,
  p previous, space pause,
  f / b jump 10 s forward / back, Esc or q stop.

  HOST is an IPv4 address or a name (asked of the DNS server DHCP names, else
  the gateway); our address comes from DHCP on the NIC chosen with
  `nic use N`. FLAC (mono or stereo, up to 24 bits), MP3 (any MPEG
  layer; no gapless trim) and WAV (PCM 8-32 bit, float32); the extension
  chooses. A rate the
  device does not offer is resampled (windowed sinc); rate=HZ forces
  the output rate. Escape stops.
"#);
