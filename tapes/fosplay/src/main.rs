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

use outfx::{Bass, Layout, Options, Upmix};
use ui::{Action, Page, Ui};
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
// The device= choice: audio_stop hands the operator's device back, so a restart must pick it again.
static mut DEVICE: Option<u32> = None;
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
/// Settings changed since the file was written.
static mut DIRTY: bool = false;
/// Started with `browse`: Stop goes back to the list instead of leaving the tape.
static mut QUIT: bool = false;
/// The last share and folder browsed, `//HOST/SHARE/DIR`; kept in the settings file.
static mut SAVED_SHARE: Line<200> = Line::new();
/// The folder the browser page shows, and where (`remote`: a share, else a FOSFS or FAT volume named by `path`).
static mut BROWSER: Browser = Browser { path: Line::new(), remote: false, host: Line::new(), share: Line::new(), loaded: false, pick: Line::new(), picked: false };
const CONFIG_FILE: &str = "fosplay.cfg";

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

fn reselect_device(api: &TapeApi) {
    if let Some(number) = unsafe { DEVICE } {
        (api.audio_use_device)(number);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if let Some(rest) = core::str::from_utf8(tape_rt::args()).unwrap_or("").trim_start().strip_prefix("device=") {
        let word = rest.split_whitespace().next().unwrap_or("");
        unsafe { DEVICE = tape_rt::audio_device_number(api, word).ok() };
    }
    let arguments = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(rest) => rest,
        Err(reason) => return println!("fosplay: {reason}."),
    };
    let mut arguments = arguments;
    load_settings(api);
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
    if command.is_empty() {
        return browse_command(api, words);
    }
    if command == "play" {
        play_command(api, words);
        if unsafe { DIRTY } {
            save_settings(api);
        }
        return;
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
            "volume" => match tape_rt::parse_f32(value) {
                Some(volume) if (0.0..=1.0).contains(&volume) => VOLUME = volume,
                _ => return Err("volume= needs 0 to 1"),
            },
            "share" => {
                SAVED_SHARE.clear();
                let _ = core::fmt::Write::write_str(&mut SAVED_SHARE, value);
            }
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
    println!("       fosplay [device=N|loopN] [OPTIONS] [browse [//HOST/SHARE[/DIR]|local]]");
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
    /// The browser picked something: the playlist is rebuilt from its folder (`take_pick`).
    Load,
    /// Leave the tape.
    Quit,
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
    /// `UPMIX` feeds the first channels of the layout (5.1: L R C LFE Ls Rs); the device has that many.
    upmix: bool,
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

/// What the browser page shows: a folder of a share (`remote`) or a volume of this machine.
struct Browser {
    path: Line<200>,
    remote: bool,
    host: Line<64>,
    share: Line<64>,
    /// `ENTRIES` holds `path`'s listing.
    loaded: bool,
    /// What a tap chose to play, `""` for the whole folder; `picked` says there is one.
    pick: Line<132>,
    picked: bool,
}

fn browser() -> &'static mut Browser {
    #[allow(static_mut_refs)]
    unsafe { &mut *core::ptr::addr_of_mut!(BROWSER) }
}

fn client() -> &'static mut Player {
    #[allow(static_mut_refs)]
    unsafe { &mut *core::ptr::addr_of_mut!(CLIENT) }
}

fn effects() -> &'static mut Options {
    #[allow(static_mut_refs)]
    unsafe { &mut *core::ptr::addr_of_mut!(EFFECTS) }
}

/// Applies a settings action (a slider, a switch) to what is remembered; the sound follows via `Output::sync_effects`.
fn change_setting(action: Action, device_channels: usize) {
    let effects = effects();
    match action {
        Action::Volume(slider) => unsafe { VOLUME = slider.clamp(0.0, 1.0) },
        Action::Bass(db) => effects.bass.gain_db = db.clamp(0.0, ui::BASS_MAX_DB),
        Action::BassHz(hz) => effects.bass.hz = hz.clamp(ui::BASS_HZ_RANGE.0, ui::BASS_HZ_RANGE.1),
        Action::BassQ(q) => effects.bass.q = q.clamp(ui::BASS_Q_RANGE.0, ui::BASS_Q_RANGE.1),
        Action::BassMgmt => effects.surround.bass_mgmt = !effects.surround.bass_mgmt,
        Action::Width(width) => effects.surround.width = width.clamp(0.0, 1.0),
        Action::Lfe(hz) => effects.surround.lfe_hz = hz.clamp(ui::LFE_RANGE.0, ui::LFE_RANGE.1),
        Action::Delay(ms) => effects.surround.delay_ms = ms.clamp(ui::DELAY_RANGE.0, ui::DELAY_RANGE.1),
        Action::Center => effects.surround.center = !effects.surround.center,
        Action::Upmix => effects.upmix = !effects.upmix,
        Action::Layout => {
            // The next layout the device has the outputs for.
            let all = Layout::ALL;
            let at = all.iter().position(|layout| *layout == effects.surround.layout).unwrap_or(0);
            if let Some(next) = (1..=all.len()).map(|step| all[(at + step) % all.len()]).find(|layout| layout.channels() <= device_channels) {
                // Bass management is what a stereo pair with a sub is for; the other layouts start without it.
                effects.surround.bass_mgmt = next == Layout::Stereo21;
                effects.surround.layout = next;
            }
        }
        _ => return,
    }
    unsafe { DIRTY = true };
}

/// Copies the settings (and what the device allows) to the screen.
fn show_settings(view: &mut ui::View, device_channels: usize) {
    let effects = *effects();
    let layout = effects.surround.layout;
    view.volume = unsafe { VOLUME };
    view.bass_db = effects.bass.gain_db;
    view.bass_hz = effects.bass.hz;
    view.bass_q = effects.bass.q;
    view.bass_mgmt = effects.surround.bass_mgmt;
    view.has_lfe = layout.has_lfe();
    view.has_surround = layout.has_surround();
    view.width = effects.surround.width;
    view.lfe_hz = effects.surround.lfe_hz;
    view.delay_ms = effects.surround.delay_ms;
    view.center = effects.surround.center;
    view.layout = layout.name();
    view.has_center = layout.has_center();
    view.upmix_ok = layout.channels() <= device_channels;
    view.upmix = effects.upmix && view.upmix_ok;
}

/// A layout the device can carry (the saved one may be too wide for this device); false when none fits.
fn fit_layout(device_channels: usize) -> bool {
    let effects = effects();
    if effects.surround.layout.channels() <= device_channels {
        return true;
    }
    match Layout::fitting(device_channels) {
        Some(layout) => {
            effects.surround.layout = layout;
            true
        }
        None => false,
    }
}

fn draw_ui(ui: &mut Ui) {
    #[allow(static_mut_refs)]
    let (list, entries) = unsafe { (&*core::ptr::addr_of!(PLAYLIST), &*core::ptr::addr_of!(ENTRIES)) };
    ui.draw(
        |index| {
            let name = list.get(index);
            let name = name.rsplit('/').next().unwrap_or(name);
            name.rsplit_once('.').map_or(name, |(stem, _)| stem)
        },
        |index| match entries.get(index) {
            "../" => ".. (up)",
            name => name,
        },
    );
}

/// Work that follows a page change: the browser lists its folder when first shown, and leaving the settings saves them.
fn page_changed(api: &TapeApi, ui: &mut Ui) {
    static mut LAST: Page = Page::Player;
    let page = ui.view.page;
    unsafe {
        if LAST == Page::Settings && page != Page::Settings && DIRTY {
            save_settings(api);
        }
        LAST = page;
    }
    if page == Page::Browse && !browser().loaded {
        browser_load(api, ui, "");
    }
}

/// `fosplay.cfg`: one `key=value` per line, the command-line options plus `volume` and `share`.
fn save_settings(api: &TapeApi) {
    use core::fmt::Write;
    let effects = *effects();
    let mut body = Line::<1024>::new();
    let _ = write!(body, "# fosplay settings, rewritten when they change\n");
    let _ = write!(body, "volume={:.2}\nbass={:.1}\nbassfreq={:.0}\nbassq={:.2}\n", unsafe { VOLUME }, effects.bass.gain_db, effects.bass.hz, effects.bass.q);
    let _ = write!(body, "upmix={}\ncenter={}\nwidth={:.2}\nlfe={:.0}\ndelay={:.0}\nlayout={}\n", if effects.upmix { "on" } else { "off" }, if effects.surround.center { "on" } else { "off" },
                   effects.surround.width, effects.surround.lfe_hz, effects.surround.delay_ms, effects.surround.layout.name());
    let _ = write!(body, "bassmgmt={}\n", if effects.surround.bass_mgmt { "on" } else { "off" });
    #[allow(static_mut_refs)]
    let share = unsafe { SAVED_SHARE.as_str() };
    if !share.is_empty() {
        let _ = write!(body, "share={share}\n");
    }
    if (api.fs_write)(CONFIG_FILE.as_ptr(), CONFIG_FILE.len(), body.as_str().as_ptr(), body.as_str().len()) == 0 {
        unsafe { DIRTY = false };
    }
}

/// Reads `fosplay.cfg` if there is one; a line it does not understand is skipped.
fn load_settings(api: &TapeApi) {
    let capacity = 4096;
    let buffer = (api.alloc)(capacity);
    if buffer == 0 {
        return;
    }
    let length = (api.fs_read)(CONFIG_FILE.as_ptr(), CONFIG_FILE.len(), buffer as *mut u8, capacity);
    if length > 0 {
        let text = unsafe { core::slice::from_raw_parts(buffer as *const u8, length as usize) };
        for line in core::str::from_utf8(text).unwrap_or("").lines() {
            if let Some((key, value)) = line.split_once('=') {
                let _ = set_option(key.trim(), value.trim());
            }
        }
    }
    (api.free)(buffer, capacity);
}

/// Points the browser at `path` on a share (or, without one, on a volume of this machine) and remembers the share.
fn set_browser(share: Option<(&str, &str)>, path: &str) {
    use core::fmt::Write;
    let browser = browser();
    let path = path.trim_matches('/');
    browser.path.clear();
    let _ = write!(browser.path, "{path}");
    browser.loaded = false;
    browser.host.clear();
    browser.share.clear();
    browser.remote = share.is_some();
    if let Some((host, name)) = share {
        let _ = write!(browser.host, "{host}");
        let _ = write!(browser.share, "{name}");
        #[allow(static_mut_refs)]
        unsafe {
            SAVED_SHARE.clear();
            let _ = write!(SAVED_SHARE, "//{host}/{name}{}{path}", if path.is_empty() { "" } else { "/" });
            DIRTY = true;
        }
    }
}

/// Fills `ENTRIES` with the browser's folder and tells the screen.
/// `select` is the entry (a folder, `NAME/`) to put the cursor on, when coming back out of it.
fn browser_load(api: &TapeApi, ui: &mut Ui, select: &str) {
    use core::fmt::Write;
    let browser = browser();
    ui.view.message.clear();
    load_entries(api, browser.remote.then(client), browser.path.as_str());
    #[allow(static_mut_refs)]
    let count = unsafe { (*core::ptr::addr_of!(ENTRIES)).count };
    ui.view.entries = count;
    ui.view.folder.clear();
    if browser.remote {
        let _ = write!(ui.view.folder, "//{}/{}/{}", browser.host.as_str(), browser.share.as_str(), browser.path.as_str());
    } else {
        let _ = write!(ui.view.folder, "{}", if browser.path.is_empty() { "FOSFS" } else { browser.path.as_str() });
    }
    ui.reset_list();
    #[allow(static_mut_refs)]
    let entries = unsafe { &*core::ptr::addr_of!(ENTRIES) };
    if let Some(found) = (0..entries.count).find(|&index| !select.is_empty() && entries.get(index).strip_suffix('/') == Some(select)) {
        ui.select(found);
    }
    browser.loaded = true;
    if unsafe { DIRTY } && browser.remote {
        save_settings(api);
    }
}

/// A tap or key in the browser. True when it chose something to play (`take_pick` then builds the playlist).
fn browser_action(api: &TapeApi, action: Action) -> bool {
    use core::fmt::Write;
    let browser = browser();
    let Some(ui) = ui() else { return false };
    #[allow(static_mut_refs)]
    let entries = unsafe { &*core::ptr::addr_of!(ENTRIES) };
    let mut next = Line::<200>::new();
    let remote = browser.remote;
    match action {
        Action::PlayAll => {
            browser.pick.clear();
            browser.picked = true;
            return true;
        }
        Action::Parent if browser.remote => {
            let _ = write!(next, "{}", parent_of(browser.path.as_str()));
        }
        Action::Open(index) if index < entries.count => {
            let name = entries.get(index);
            if name == "../" {
                let _ = write!(next, "{}", parent_of(browser.path.as_str()));
            } else if let Some(folder) = name.strip_suffix('/') {
                let join = if browser.path.is_empty() || !browser.remote { "" } else { "/" };
                let _ = write!(next, "{}{join}{folder}", browser.path.as_str());
            } else {
                browser.pick.clear();
                let _ = write!(browser.pick, "{name}");
                browser.picked = true;
                return true;
            }
        }
        _ => return false,
    }
    let mut host = Line::<64>::new();
    let mut name = Line::<64>::new();
    let _ = write!(host, "{}", browser.host.as_str());
    let _ = write!(name, "{}", browser.share.as_str());
    // Going up: the folder we leave is where the cursor lands.
    let mut left = Line::<132>::new();
    if matches!(action, Action::Parent) || matches!(action, Action::Open(_)) && next.as_str() == parent_of(browser.path.as_str()) {
        let _ = write!(left, "{}", browser.path.as_str().rsplit('/').next().unwrap_or(""));
    }
    set_browser(remote.then(|| (host.as_str(), name.as_str())), next.as_str());
    browser_load(api, ui, left.as_str());
    false
}

/// Builds the playlist from what `browser_action` chose and returns where in it to start; `None` (with a message on the
/// screen) when the folder has no audio files.
fn take_pick() -> Option<usize> {
    let browser = browser();
    if !core::mem::take(&mut browser.picked) {
        return None;
    }
    let start = playlist_from_entries(browser.path.as_str(), browser.pick.as_str());
    #[allow(static_mut_refs)]
    if unsafe { (*core::ptr::addr_of!(PLAYLIST)).count } == 0 {
        show_message(format_args!("no audio files here"));
        return None;
    }
    if let Some(ui) = ui() {
        ui.set_page(Page::Player);
    }
    Some(start)
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
        let used = effects().surround.layout.channels();
        let wide = if self.upmix {
            #[allow(static_mut_refs)]
            Some(unsafe { (*core::ptr::addr_of_mut!(UPMIX)).process(frame) })
        } else {
            None
        };
        for channel in 0..self.channels {
            let value = match wide {
                Some(six) => if channel < used { six[channel] } else { 0.0 },
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
        match action {
            Action::Previous => self.command = Some(Outcome::Previous),
            Action::Next => self.command = Some(Outcome::Next),
            Action::Back => self.command = Some(Outcome::Skip(-SKIP_SECONDS)),
            Action::Forward => self.command = Some(Outcome::Skip(SKIP_SECONDS)),
            Action::Stop => self.command = Some(Outcome::Stop),
            Action::PlayPause => self.paused = !self.paused,
            Action::Seek(permille) => self.command = Some(Outcome::SeekTo(permille)),
            Action::Jump(index) => self.command = Some(Outcome::Jump(index)),
            Action::Quit => self.command = Some(Outcome::Quit),
            Action::Open(_) | Action::Parent | Action::PlayAll => {
                if browser_action(self.api, action) {
                    self.command = Some(Outcome::Load);
                }
            }
            _ => {
                change_setting(action, self.channels);
                self.gain = volume_gain(unsafe { VOLUME });
                self.sync_effects();
            }
        }
    }

    /// Brings the bass filter and the upmix in line with `EFFECTS` after a setting changed, keeping their history.
    fn sync_effects(&mut self) {
        #[allow(static_mut_refs)]
        let effects = unsafe { &*core::ptr::addr_of!(EFFECTS) };
        match self.bass.as_mut() {
            Some(bass) if effects.bass.gain_db > 0.0 => bass.set(effects.bass),
            _ => self.bass = Bass::new(self.rate, effects.bass),
        }
        let fits = effects.surround.layout.channels() <= self.channels;
        #[allow(static_mut_refs)]
        let upmix = unsafe { &mut *core::ptr::addr_of_mut!(UPMIX) };
        match (effects.upmix && fits, self.upmix) {
            (true, false) => upmix.configure(self.rate, effects.surround),
            (true, true) => upmix.tune(self.rate, effects.surround),
            _ => {}
        }
        self.upmix = effects.upmix && fits;
    }

    /// Fingers and the picture: runs often, draws at most every 50 ms and only when something changed.
    fn service_ui(&mut self) {
        #[allow(static_mut_refs)]
        let Some(ui) = (unsafe { (*core::ptr::addr_of_mut!(UI)).as_mut() }) else { return };
        while let Some(action) = ui.next_action() {
            self.apply(action);
        }
        page_changed(self.api, ui);
        if !ui.due() {
            return;
        }
        let mut info = AudioInfo::default();
        (self.api.audio_info)(&mut info);
        let queued = (self.empty.saturating_sub(info.playback_space as usize) / (self.channels * self.slot).max(1)) as f32 / self.rate.max(1) as f32;
        let view = &mut ui.view;
        view.seconds = (self.decoded - queued).max(0.0);
        view.paused = self.paused;
        view.level = [view.level[0] * 0.8, view.level[1] * 0.8];
        view.level = [view.level[0].max(self.peak[0]), view.level[1].max(self.peak[1])];
        self.peak = [0.0; 2];
        show_settings(view, self.channels);
        draw_ui(ui);
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
    set_browser(Some((first, share)), if is_audio(path) { parent_of(path) } else { path });
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
    if unsafe { DIRTY } {
        save_settings(api);
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
    let mut session = Session { api, output: None, wanted: 0, out_rate: 0, empty: 0, started: 0, tracks: 0 };
    let mut index = start;
    let mut stopped = false;
    #[allow(static_mut_refs)]
    let own_ui = unsafe { (*core::ptr::addr_of!(UI)).is_none() };
    #[allow(static_mut_refs)]
    unsafe {
        if own_ui {
            *core::ptr::addr_of_mut!(UI) = if GFX { Ui::open(api, UI_SCREEN, Page::Player) } else { None };
        }
    }
    loop {
        // The browser may have replaced the playlist while a track played, so look again every time.
        #[allow(static_mut_refs)]
        let list = unsafe { &*core::ptr::addr_of!(PLAYLIST) };
        let total = list.count;
        if index >= total {
            break;
        }
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
            Outcome::Load => match take_pick() {
                Some(first) => index = first,
                None => {}
            },
            Outcome::Stop => {
                stopped = true;
                break;
            }
            Outcome::Quit => {
                stopped = true;
                unsafe { QUIT = true };
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

/// `fosplay [browse [//HOST/SHARE[/DIR]]]` (or `HOST SHARE [DIR]`); `browse local [VOLUME]` the files of this machine. With no
/// share it goes back to the last one used. The touch screen lists folders and audio files; a tap on a folder enters it,
/// on a file plays the folder from there.
fn browse_command<'a>(api: &'static TapeApi, mut words: impl Iterator<Item = &'a str>) {
    if !unsafe { GFX } {
        return println!("fosplay: browse needs the touch screen (gfx=off is set).");
    }
    let first = words.next().unwrap_or("");
    let mut saved = Line::<200>::new();
    #[allow(static_mut_refs)]
    let _ = core::fmt::Write::write_str(&mut saved, unsafe { SAVED_SHARE.as_str() });
    let (target, volume) = if first == "local" {
        (None, words.next().unwrap_or(""))
    } else if first.is_empty() {
        (parse_share(saved.as_str(), &mut core::iter::empty()), "")
    } else {
        (parse_share(first, &mut words), first)
    };
    if let Some((host, share, _)) = target {
        if host.is_empty() || share.is_empty() || open(api, client(), host, share).is_none() {
            return;
        }
    }
    let Some(opened) = Ui::open(api, unsafe { UI_SCREEN }, Page::Browse) else {
        return println!("fosplay: browse needs a pixel screen.");
    };
    unsafe { *core::ptr::addr_of_mut!(UI) = Some(opened) };
    set_browser(target.map(|(host, share, _)| (host, share)), target.map_or(volume, |(_, _, path)| path));
    browse(api);
    #[allow(static_mut_refs)]
    if let Some(ui) = unsafe { (*core::ptr::addr_of_mut!(UI)).take() } {
        ui.close();
    }
    if unsafe { DIRTY } {
        save_settings(api);
    }
    if target.is_some() {
        client().transport().socket.close(api);
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

/// The browser with nothing playing: the browser and settings pages on their own, until Quit.
fn browse(api: &'static TapeApi) {
    loop {
        let Some(ui) = ui() else { return };
        let channels = || {
            let mut info = AudioInfo::default();
            (api.audio_info)(&mut info);
            info.playback_channels as usize
        };
        let mut action = ui.next_action();
        if action.is_none() && ui.view.page == Page::Browse {
            action = match (api.read_key)() {
                KEY_NONE => None,
                KEY_ABORT => Some(Action::Quit),
                key => match (key - KEY_BASE) as u8 {
                    b'q' => Some(Action::Quit),
                    b'j' => {
                        ui.cursor_by(1);
                        None
                    }
                    b'k' => {
                        ui.cursor_by(-1);
                        None
                    }
                    b'\r' | b'\n' => (ui.view.cursor < ui.view.entries).then_some(Action::Open(ui.view.cursor)),
                    0x08 | 0x7f => Some(Action::Parent),
                    b'a' => Some(Action::PlayAll),
                    b's' => {
                        ui.open_settings();
                        None
                    }
                    _ => None,
                },
            };
        }
        page_changed(api, ui);
        match action {
            Some(Action::Quit) => return,
            Some(action @ (Action::Open(_) | Action::Parent | Action::PlayAll)) => {
                if browser_action(api, action) {
                    if let Some(start) = take_pick() {
                        play_picked(api, start);
                        if unsafe { QUIT } {
                            return;
                        }
                    }
                }
            }
            Some(action) => change_setting(action, channels()),
            None => {}
        }
        let Some(ui) = self::ui() else { return };
        if ui.due() {
            show_settings(&mut ui.view, channels());
            draw_ui(ui);
        } else if action.is_none() {
            tape_rt::idle();
        }
    }
}

/// Plays the playlist from `start` over the browser, then goes back to it.
fn play_picked(api: &'static TapeApi, start: usize) {
    let remote = browser().remote;
    run(api, remote.then(client), start);
    if let Some(ui) = ui() {
        ui.view.playing = false;
        ui.set_page(Page::Browse);
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
            reselect_device(api);
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
        let upmix = effects.upmix && fit_layout(info.playback_channels as usize);
        if effects.upmix && !upmix {
            println!("fosplay: upmix needs 3 playback channels; the device has {} (`audio modes`, `audio use playback N`): stereo.", info.playback_channels);
        }
        let effects = unsafe { EFFECTS };
        if upmix {
            #[allow(static_mut_refs)]
            unsafe { (*core::ptr::addr_of_mut!(UPMIX)).configure(session.out_rate, effects.surround) };
            println!("fosplay: upmix {} on {} channels.", effects.surround.layout.name(), info.playback_channels);
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
        view.playing = true;
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
                        // The ring holds seconds of the old position; stopping the device empties it.
                        (api.audio_stop)();
                        reselect_device(api);
                        let status = (api.audio_start)(out_rate, 0, 1);
                        if status != 0 {
                            failure = Some("cannot restart playback after the seek");
                            break 'blocks;
                        }
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
  meter and volume, with buttons for two more pages: Browse (folders and
  audio files: touch a folder to open it, a file to play its folder from
  there, Play all, Up) and Settings (bass boost, upmix switch, layout, width,
  LFE crossover, delay, centre). gfx=off hides the panel, screen=N picks the
  screen.
  Plain `fosplay`, or `browse` without a share, opens the browser on the last
  share and folder used (`browse local [diskN]` shows FOSFS or a FAT volume).
  Keys in the browser: j k move, Enter opens, Backspace up, a play all, s
  settings, q quits. Stop returns to the list.
  Volume, bass, upmix settings and the last share are saved in the file
  fosplay.cfg (key=value lines) when they change; command-line options win.

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
    upmix=on      stereo to surround on the device's first channels (pick a
                  multichannel mode: `audio use playback N`)
    layout=5.1    L R C LFE Ls Rs; 4.1: L R LFE Ls Rs (no centre); 4.0: L R Ls
                  Rs (quad); 2.1: L R LFE (turns bassmgmt on)
    bassmgmt=on   high-pass the fronts and centre at lfe=, the LFE gets their bass
    center=off    leave the centre silent, fronts untouched (default on)
    width=0..1    surround level (0.7), lfe=HZ crossover 40-200 (100),
    delay=MS      surround delay 0-40 (15)
"#);
