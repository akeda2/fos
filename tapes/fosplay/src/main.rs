//! fosplay: the media player. `fosplay ls HOST SHARE [DIR]` lists a directory of a guest-accessible SMB share,
//! `fosplay cat HOST SHARE FILE` reads a file to the end and reports its size, speed and a checksum (FNV-1a 32), and
//! `fosplay [device=N|loopN] play HOST SHARE FILE` decodes a FLAC file while it streams in and plays it. All of them use
//! `crates/smb` over the shared TCP client (`tape_ip::tcp`), on the interface chosen with `nic use N` and an address from
//! DHCP. HOST is an IPv4 address.
//!
//! The player is hard-float: FLAC decoding is integer (`audio_decode::flac`), MP3 is float (`audio_decode::mp3`), the conversion to the device's format and the
//! resampling to a rate the device offers (cubic, `audio_decode::resample`) are `f32`. Nothing is printed between the first
//! and the last sample: a console line costs ~10 ms on real hardware.
#![no_std]
#![no_main]

use audio_decode::{Decode, Source, SourceError, flac, mp3, resample::Resampler};
use smb::{Client, Error, Handle, Transport};
use tape_ip::net;
use tape_ip::tcp::Socket;
use tape_net::{Ipv4Address, MacAddress};
use tape_rt::{AudioInfo, AudioStats, TapeApi, println};

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

fn is_mp3(path: &str) -> bool {
    path.len() > 4 && path.as_bytes()[path.len() - 4..].eq_ignore_ascii_case(b".mp3")
}

/// The source of the decoder that handles `path` (chosen by its extension).
fn media_source(path: &str) -> &'static mut MediaSource {
    #[allow(static_mut_refs)]
    unsafe {
        if is_mp3(path) { (*core::ptr::addr_of_mut!(MP3)).source() } else { (*core::ptr::addr_of_mut!(FLAC)).source() }
    }
}

fn decoder_for(path: &str) -> &'static mut dyn Decode {
    #[allow(static_mut_refs)]
    unsafe {
        if is_mp3(path) { &mut *core::ptr::addr_of_mut!(MP3) } else { &mut *core::ptr::addr_of_mut!(FLAC) }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(rest) => rest,
        Err(reason) => return println!("fosplay: {reason}."),
    };
    let mut words = arguments.split_whitespace();
    let command = words.next().unwrap_or("");
    let rest: [&str; 3] = core::array::from_fn(|_| words.next().unwrap_or(""));
    if command == "play" && !rest[0].is_empty() && rest[1].is_empty() {
        return play_local(api, rest[0]);
    }
    let [host, share, path] = rest;
    let Some(address) = net::parse_ip(host) else { return usage() };
    if share.is_empty() || !matches!(command, "ls" | "cat" | "play") || (command != "ls" && path.is_empty()) {
        return usage();
    }
    #[allow(static_mut_refs)]
    let client = unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) };
    let Some(()) = open(api, client, address, host, share) else { return };
    match command {
        "ls" => list(client, path),
        "cat" => cat(api, client, path),
        _ => {
            let Some(file) = open_remote(client, path) else { return };
            media_source(path).handle = Some(file);
            play(api, path);
            let _ = client.close(&file);
        }
    }
    client.transport().socket.close(api);
}

fn usage() {
    println!("usage: fosplay ls HOST SHARE [DIR] | cat HOST SHARE FILE");
    println!("       fosplay [device=N|loopN] play FILE | play HOST SHARE FILE");
}

type Player = Client<Link, { 64 * 1024 + 1024 }>;

/// Address by DHCP, TCP to port 445, SMB negotiate, guest session, tree connect.
fn open(api: &'static TapeApi, client: &mut Player, address: Ipv4Address, host: &str, share: &str) -> Option<()> {
    let nic = tape_rt::active_nic();
    if nic >= (api.nic_count)() as usize {
        println!("fosplay: no network interface.");
        return None;
    }
    let mut mac = [0u8; 6];
    (api.nic_mac)(nic, mac.as_mut_ptr());
    let Some(config) = net::dhcp(api, nic, MacAddress(mac)) else {
        println!("fosplay: no DHCP lease on NIC {nic}.");
        return None;
    };
    println!("fosplay: {:?} -> {host}:{SMB_PORT}", config.ip);
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
struct Output<'a> {
    api: &'a TapeApi,
    format: audio_sample::Format,
    channels: usize,
    slot: usize,
    staging: [u8; 8192],
    staged: usize,
    aborted: bool,
}

impl Output<'_> {
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

    fn flush(&mut self) {
        let mut sent = 0;
        while sent < self.staged && !self.aborted {
            let accepted = (self.api.audio_write)(self.staging[sent..self.staged].as_ptr(), self.staged - sent);
            sent += accepted;
            if (self.api.should_abort)() != 0 {
                self.aborted = true;
            } else if accepted == 0 {
                tape_rt::idle();
            }
        }
        self.staged = 0;
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
fn play_local(api: &'static TapeApi, name: &str) {
    let mut capacity = 4 << 20;
    let (buffer, length) = loop {
        let buffer = (api.alloc)(capacity);
        if buffer == 0 {
            return println!("fosplay: not enough memory.");
        }
        let result = (api.fs_read)(name.as_ptr(), name.len(), buffer as *mut u8, capacity);
        if result == tape_rt::FS_TOO_BIG && capacity < (256 << 20) {
            (api.free)(buffer, capacity);
            capacity *= 4;
            continue;
        }
        if result < 0 {
            (api.free)(buffer, capacity);
            return println!("fosplay: cannot read {name} (code {result}).");
        }
        break (buffer, result as usize);
    };
    let source = media_source(name);
    source.handle = None;
    source.memory = unsafe { core::slice::from_raw_parts(buffer as *const u8, length) };
    play(api, name);
    source.memory = &[];
    (api.free)(buffer, capacity);
}

/// Plays the file behind the source `media_source(path)` was given.
fn play(api: &'static TapeApi, path: &str) {
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        return println!("fosplay: no audio device with outputs (USB audio, HD Audio or a loopN).");
    }
    let decoder = decoder_for(path);
    if let Err(error) = decoder.open() {
        return println!("fosplay: {path}: {}.", error.describe());
    }
    let rate = decoder.sample_rate();
    let offered = &info.rates[..info.rate_count as usize];
    let out_rate = if offered.is_empty() || offered.contains(&rate) { rate } else if offered.contains(&48000) { 48000 } else { offered[0] };
    let float = tape_rt::audio_float().1;
    let depth = if decoder.bits_per_sample() == 0 { "MP3" } else { "FLAC" };
    println!(
        "fosplay: {path}: {depth}, {rate} Hz, {} channel(s), {} s; to {out_rate} Hz{}, {}{}-bit.",
        decoder.channels(), decoder.total_samples() / rate as u64, if out_rate == rate { "" } else { " (resampled)" },
        if float { "float " } else { "" }, info.playback_bits
    );
    let status = (api.audio_start)(out_rate, 0, 1);
    if status != 0 {
        return println!("fosplay: cannot start playback at {out_rate} Hz (code {status}); the device offers {offered:?}.");
    }
    (api.audio_info)(&mut info);
    let empty = (info.playback_space as usize).saturating_sub(64).min((1 << 20) - 64);
    let mut output = Output {
        api,
        format: audio_sample::Format::new(info.playback_slot_bytes as usize, info.playback_bits as usize, float),
        channels: info.playback_channels as usize,
        slot: info.playback_slot_bytes as usize,
        staging: [0; 8192],
        staged: 0,
        aborted: false,
    };
    let mut resampler = Resampler::new(rate, out_rate);
    let mut pcm = [0f32; 2 * 512];
    let mut failure = None;
    let started = tape_rt::micros();
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
            if output.aborted {
                break 'blocks;
            }
            at += step;
        }
    }
    if !output.aborted {
        output.flush();
        let mut waited = 0;
        loop {
            (api.audio_info)(&mut info);
            if info.playback_space as usize >= empty || (api.should_abort)() != 0 || waited > 120_000 {
                break;
            }
            tape_rt::idle();
            waited += 1;
        }
    }
    let mut stats = AudioStats::default();
    (api.audio_stats)(&mut stats);
    (api.audio_stop)();
    let seconds = (tape_rt::micros() - started) / 1_000_000;
    match failure {
        Some(why) => println!("fosplay: stopped after {seconds} s: {why}."),
        None => println!("fosplay: {} after {seconds} s, {} packets, {} underrun(s).", if output.aborted { "stopped" } else { "finished" }, stats.playback_packets, stats.playback_underruns),
    }
}

tape_rt::tape_help!(br#"fosplay: play audio from an SMB share (guest access).

  tape run fosplay ls HOST SHARE [DIR]
  tape run fosplay cat HOST SHARE FILE
  tape run fosplay [device=N|loopN] play HOST SHARE FILE
  tape run fosplay [device=N|loopN] play NAME        (a FOSFS file)

  HOST is an IPv4 address; the address comes from DHCP on the NIC chosen
  with `nic use N`. FLAC (mono or stereo, up to 24 bits) and MP3 (any MPEG
  layer, by the .mp3 extension; no gapless trim). A rate the
  device does not offer is resampled. Escape stops.
"#);
