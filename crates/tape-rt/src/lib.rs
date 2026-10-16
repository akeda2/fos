//! The runtime every v2 tape shares: `_start`, a panic handler, `print!`/`println!`
//! over the kernel's `putc`, and a few helpers for the callbacks in `TapeApi`.
//!
//! A tape's `main.rs` is `#![no_std] #![no_main]` and defines
//! `#[unsafe(no_mangle)] pub extern "C" fn tape_main(api: &'static TapeApi)`; this crate
//! supplies the rest. The API pointer is also stored globally (`api()`), so helper
//! functions deep in a tape can print or poll without it being threaded through.
#![no_std]

pub mod abort;

pub use fosfor_abi::tape::*;

use core::fmt::Write;

// The entry point and the panic handler exist only in a tape build (the `os: none` tape target). A host build, `cargo test` of a
// tape's library, has std's own panic handler and a C runtime's `_start`, and no `tape_main` to call.
#[cfg(target_os = "none")]
core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    _start:
        // rdi = &TapeApi, the stack is the tape's own (already 16-byte aligned at the call).
        push rbp                 // re-align for the call below
        mov rbp, rsp
        call tape_main
        pop rbp
        ret
    "#
);

static mut API: *const TapeApi = core::ptr::null();

/// Called first thing by `tape_main`.
pub fn init(api: &'static TapeApi) {
    unsafe { API = api };
}

pub fn api() -> &'static TapeApi {
    unsafe { &*API }
}

/// The text after `tape run`.
pub fn args() -> &'static [u8] {
    let api = api();
    unsafe { core::slice::from_raw_parts(api.args, api.args_len) }
}

/// Whether the kernel's table has the version-3 audio-device calls (`audio_devices`, `audio_use_device`): a tape built for
/// 3 can still run on an older kernel, where it simply cannot choose or name the device.
pub fn has_audio_device_calls(api: &TapeApi) -> bool {
    api.version >= 3 && (api.size as usize) >= core::mem::size_of::<TapeApi>()
}

/// The audio device the `audio_*` calls currently use (`audio device N|auto`, or a tape's `device=N`), if the kernel can say.
pub fn active_audio_device(api: &TapeApi) -> Option<AudioDevice> {
    if !has_audio_device_calls(api) {
        return None;
    }
    let count = (api.audio_devices)(0, core::ptr::null_mut());
    for number in 1..=count {
        let mut device = AudioDevice::default();
        (api.audio_devices)(number, &mut device);
        if device.active == 1 {
            return Some(device);
        }
    }
    None
}

/// How to show an audio device to a person: "USB audio" or "HD Audio 00:07.0 (10de:044a)" (PCI location and IDs).
pub struct AudioDeviceName(pub AudioDevice);

impl core::fmt::Display for AudioDeviceName {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let device = &self.0;
        if device.kind == AUDIO_KIND_HDA {
            let location = device.pci_location;
            write!(out, "HD Audio {:02x}:{:02x}.{} ({:04x}:{:04x})", (location >> 16) & 0xff, (location >> 8) & 0xff, location & 7, device.vendor, device.id)
        } else {
            write!(out, "USB audio")
        }
    }
}

/// Every audio tape accepts a leading `device=N` (the number `audio` lists; 0 = automatic): it picks the audio device for
/// this run, and is removed from the arguments. Returns what is left, or why the device cannot be chosen. The choice is
/// undone by the kernel when the tape ends, so the operator's own `audio device N` is never overridden.
pub fn take_audio_device_arg<'a>(api: &TapeApi, arguments: &'a str) -> Result<&'a str, &'static str> {
    let trimmed = arguments.trim_start();
    let Some(rest) = trimmed.strip_prefix("device=") else { return Ok(arguments) };
    let (number, after) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let Ok(number) = number.parse::<u32>() else { return Err("device=N needs a number (`audio` lists the devices; 0 = automatic)") };
    if !has_audio_device_calls(api) {
        return Err("this kernel cannot choose an audio device (tape API 3 needed)");
    }
    if (api.audio_use_device)(number) != 0 {
        return Err("there is no such audio device (`audio` lists them)");
    }
    Ok(after.trim_start())
}

pub struct Console;

static mut QUIET: bool = false;
static mut LINE: [u8; 160] = [0; 160];
static mut LINE_LENGTH: usize = 0;
static mut LAST: [u8; 160] = [0; 160];
static mut LAST_LENGTH: usize = 0;

/// While quiet, `print!`/`println!` do not reach the screen: a tape that draws its own full-screen panel keeps the
/// text its worker code prints from scribbling over it. The last complete line is kept (`last_message`) so the panel
/// can show it where it wants.
pub fn set_quiet(quiet: bool) {
    unsafe { QUIET = quiet };
}

/// The last complete line printed while quiet.
pub fn last_message() -> &'static str {
    unsafe {
        let last: &'static [u8; 160] = &*(&raw const LAST);
        core::str::from_utf8(&last[..LAST_LENGTH]).unwrap_or("")
    }
}

impl Write for Console {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        if unsafe { QUIET } {
            for &byte in text.as_bytes() {
                unsafe {
                    let line: &mut [u8; 160] = &mut *(&raw mut LINE);
                    if byte == b'\n' {
                        if LINE_LENGTH > 0 {
                            let last: &mut [u8; 160] = &mut *(&raw mut LAST);
                            last[..LINE_LENGTH].copy_from_slice(&line[..LINE_LENGTH]);
                            LAST_LENGTH = LINE_LENGTH;
                        }
                        LINE_LENGTH = 0;
                    } else if byte >= b' ' && LINE_LENGTH < 160 {
                        line[LINE_LENGTH] = byte;
                        LINE_LENGTH += 1;
                    }
                }
            }
            return Ok(());
        }
        if api().version >= 2 {
            (api().puts)(text.as_ptr(), text.len());
            return Ok(());
        }
        for &byte in text.as_bytes() {
            (api().putc)(u32::from(byte));
        }
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($argument:tt)*) => {{ let _ = core::fmt::Write::write_fmt(&mut $crate::Console, format_args!($($argument)*)); }};
}

#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($argument:tt)*) => {{ $crate::print!($($argument)*); $crate::print!("\n"); }};
}

/// Prints raw bytes through a `putc` callback (kept for tapes ported from v1, which pass
/// the callback around explicitly).
pub fn print_bytes(putc: extern "C" fn(u32), bytes: &[u8]) {
    for &byte in bytes {
        putc(u32::from(byte));
    }
}

/// Prints an unsigned integer in decimal through a `putc` callback.
pub fn print_u32(putc: extern "C" fn(u32), mut value: u32) {
    let mut digits = [0u8; 10];
    let mut count = 0;
    loop {
        digits[count] = b'0' + (value % 10) as u8;
        count += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for &digit in digits[..count].iter().rev() {
        putc(u32::from(digit));
    }
}

/// The v1 tape ABI was a long list of cdecl parameters with 32-bit addresses; tapes ported from
/// it keep their bodies and use these functions, which have the v1 names and shapes but take
/// 64-bit pointers (as `usize`) and go through the v2 table. Not for new tapes: use `api()`.
pub mod compat {
    use super::api;
    // (A pattern `FS_NOT_FOUND =>` with the constant not in scope is a catch-all binding: every
    // error used to be reported as "not found".)
    use fosfor_abi::tape::FS_NOT_FOUND;

    pub fn should_abort() -> u32 {
        (api().should_abort)()
    }
    pub fn read_key() -> u32 {
        (api().read_key)()
    }
    pub fn wait_for_event() {
        (api().wait_for_event)();
    }
    pub fn ticks() -> u64 {
        (api().ticks)()
    }
    pub fn micros() -> u64 {
        (api().micros)()
    }
    pub fn alloc(bytes: u32) -> usize {
        (api().alloc)(bytes as usize)
    }
    pub fn free(address: usize, bytes: u32) {
        (api().free)(address, bytes as usize)
    }
    pub fn nic_count() -> u32 {
        (api().nic_count)() as u32
    }
    /// The "active" interface of v1: the one the operator chose with `nic use N` (interface 0 by default; the kernel's
    /// `nic_active`, ABI version 4).
    fn active() -> usize {
        let table = api();
        if table.version >= 4 && (table.size as usize) >= core::mem::size_of::<crate::TapeApi>() { (table.nic_active)() as usize } else { 0 }
    }
    pub fn nic_mac(out: usize) -> u32 {
        (api().nic_mac)(active(), out as *mut u8)
    }
    pub fn nic_send(frame: usize, length: u32) -> u32 {
        (api().nic_send)(active(), frame as *const u8, length as usize)
    }
    pub fn nic_receive(buffer: usize, capacity: u32) -> u32 {
        (api().nic_receive)(active(), buffer as *mut u8, capacity as usize) as u32
    }
    pub fn nic_mac_at(nic: u32, out: usize) -> u32 {
        (api().nic_mac)(nic as usize, out as *mut u8)
    }
    pub fn nic_send_at(nic: u32, frame: usize, length: u32) -> u32 {
        (api().nic_send)(nic as usize, frame as *const u8, length as usize)
    }
    pub fn nic_receive_at(nic: u32, buffer: usize, capacity: u32) -> u32 {
        (api().nic_receive)(nic as usize, buffer as *mut u8, capacity as usize) as u32
    }
    pub fn nic_set_promiscuous_at(nic: u32, on: u32) -> u32 {
        (api().nic_set_promiscuous)(nic as usize, on)
    }
    pub fn nic_max_frame_len() -> u32 {
        (api().nic_max_frame_len)() as u32
    }
    pub fn set_input_mode(mode: u32) {
        (api().set_input_mode)(mode)
    }

    /// v1 `fs_write(name, name_len, data, len)`: 0 on success.
    pub fn fs_write(name: usize, name_length: u32, data: usize, length: u32) -> u32 {
        u32::from((api().fs_write)(name as *const u8, name_length as usize, data as *const u8, length as usize) != 0)
    }
    /// v1 `fs_file_length(name, name_len, out_u64)`: status 0 ok, 1 not found (the lease-file
    /// statuses router uses), 2 other failure.
    pub fn fs_file_length(name: usize, name_length: u32, out: usize) -> u32 {
        // v2 has no length call: read into a scratch region to learn it (lease files are small).
        let capacity = 16 * 1024;
        let scratch = (api().alloc)(capacity);
        if scratch == 0 {
            return 2;
        }
        let result = (api().fs_read)(name as *const u8, name_length as usize, scratch as *mut u8, capacity);
        (api().free)(scratch, capacity);
        match result {
            length if length >= 0 => {
                unsafe { (out as *mut u64).write_unaligned(length as u64) };
                0
            }
            FS_NOT_FOUND => 1,
            _ => 2,
        }
    }
    /// v1 `fs_read(name, name_len, buffer, capacity, out_len_u32)`: status 0 ok, 1 not found.
    pub fn fs_read(name: usize, name_length: u32, buffer: usize, capacity: u32, out_length: usize) -> u32 {
        match (api().fs_read)(name as *const u8, name_length as usize, buffer as *mut u8, capacity as usize) {
            length if length >= 0 => {
                unsafe { (out_length as *mut u32).write_unaligned(length as u32) };
                0
            }
            FS_NOT_FOUND => 1,
            _ => 2,
        }
    }
    pub fn fs_remove(name: usize, name_length: u32) -> u32 {
        u32::from((api().fs_remove)(name as *const u8, name_length as usize) != 0)
    }
    pub fn terminal_position() -> u32 {
        0
    }

    // v1 tapes registered hooks with tape-rt; the v2 table makes that unnecessary.
    pub fn set_wait_hook(_hook: extern "C" fn()) {}
    pub fn set_micros_hook(_hook: extern "C" fn() -> u64) {}
}

/// The v1 `set_input_mode`/`set_raw_keys` pair, kept so ported tapes compile unchanged. On v2
/// the kernel never erases on Backspace for a tape, so every tape is already in "raw" mode and
/// does its own echo and erasing.
pub fn set_input_mode_hook(_hook: extern "C" fn(u32)) {}
pub fn set_raw_keys(_raw: bool) {}

/// A decoded key. Cursor keys arrive from `read_key` as ANSI escape sequences
/// (`ESC [ D` is Left), identically from the local PS/2 keyboard and from a
/// remote `term net` terminal; `KeyDecoder` turns the byte stream into these.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A printable ASCII character.
    Char(u8),
    Enter,
    Backspace,
    Tab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    Insert,
    PageUp,
    PageDown,
    /// Another control byte (`1` is Ctrl-A, `5` Ctrl-E, ...).
    Control(u8),
}

#[derive(Clone, Copy)]
enum DecoderState {
    Ground,
    Escape,
    /// Inside `ESC [`, accumulating the numeric parameter.
    Csi(u16),
    /// After `ESC O` (application-mode cursor keys, which some terminals send).
    Ss3,
}

/// Incremental ANSI key decoder: feed it one byte per `read_key` event. A
/// sequence can span several calls, so it keeps its state between them and
/// needs no timeouts -- a lone Escape never reaches a tape (the kernel turns
/// it into the abort event), so every `0x1b` byte starts a sequence.
pub struct KeyDecoder {
    state: DecoderState,
}

impl KeyDecoder {
    pub const fn new() -> Self {
        Self {
            state: DecoderState::Ground,
        }
    }

    /// Returns a key once `byte` completes one; `None` while mid-sequence or
    /// for a sequence this decoder does not know (which is dropped).
    pub fn feed(&mut self, byte: u8) -> Option<Key> {
        match self.state {
            DecoderState::Ground => match byte {
                0x1b => {
                    self.state = DecoderState::Escape;
                    None
                }
                b'\r' | b'\n' => Some(Key::Enter),
                8 | 127 => Some(Key::Backspace),
                b'\t' => Some(Key::Tab),
                32..=126 => Some(Key::Char(byte)),
                _ => Some(Key::Control(byte)),
            },
            DecoderState::Escape => match byte {
                b'[' => {
                    self.state = DecoderState::Csi(0);
                    None
                }
                b'O' => {
                    self.state = DecoderState::Ss3;
                    None
                }
                _ => {
                    // Not a sequence we know: forget the Escape and treat the
                    // byte as an ordinary key.
                    self.state = DecoderState::Ground;
                    self.feed(byte)
                }
            },
            DecoderState::Csi(parameter) => match byte {
                b'0'..=b'9' => {
                    self.state = DecoderState::Csi(
                        parameter.saturating_mul(10).saturating_add(u16::from(byte - b'0')),
                    );
                    None
                }
                // Modifier parameters (`ESC [ 1 ; 5 C`): keep only the last
                // number's final key, ignoring which modifier was held.
                b';' => {
                    self.state = DecoderState::Csi(0);
                    None
                }
                _ => {
                    self.state = DecoderState::Ground;
                    match byte {
                        b'A' => Some(Key::Up),
                        b'B' => Some(Key::Down),
                        b'C' => Some(Key::Right),
                        b'D' => Some(Key::Left),
                        b'H' => Some(Key::Home),
                        b'F' => Some(Key::End),
                        b'~' => match parameter {
                            1 | 7 => Some(Key::Home),
                            2 => Some(Key::Insert),
                            3 => Some(Key::Delete),
                            4 | 8 => Some(Key::End),
                            5 => Some(Key::PageUp),
                            6 => Some(Key::PageDown),
                            _ => None,
                        },
                        _ => None,
                    }
                }
            },
            DecoderState::Ss3 => {
                self.state = DecoderState::Ground;
                match byte {
                    b'A' => Some(Key::Up),
                    b'B' => Some(Key::Down),
                    b'C' => Some(Key::Right),
                    b'D' => Some(Key::Left),
                    b'H' => Some(Key::Home),
                    b'F' => Some(Key::End),
                    _ => None,
                }
            }
        }
    }
}

static mut IDLE_HOOK: Option<fn()> = None;

/// Runs `hook` after every `idle()` (a tape running two cooperative tasks switches between them there).
pub fn set_idle_hook(hook: Option<fn()>) {
    unsafe { IDLE_HOOK = hook };
}

/// Sleeps until the next interrupt; call it when a polling pass found nothing to do.
pub fn idle() {
    (api().wait_for_event)();
    if let Some(hook) = unsafe { IDLE_HOOK } {
        hook();
    }
}

pub fn ticks() -> u64 {
    (api().ticks)()
}

/// The local time of day, if the machine's clock is set (a kernel without the call, or a dead RTC battery, gives None).
pub fn wall_clock() -> Option<WallClock> {
    let table = api();
    if (table.size as usize) < core::mem::size_of::<TapeApi>() {
        return None;
    }
    let mut now = WallClock::default();
    ((table.wall_clock)(&mut now) != 0).then_some(now)
}

/// Gives the tape a help screen the kernel can show without running it: `help NAME`, `NAME help` and `NAME --help`
/// print this text (the kernel reads the `.fos_help` section of the tape's ELF file). Use once, at file level, with a
/// byte string of lines of at most 79 columns: `tape_rt::tape_help!(br#"name: what it does ..."#);`
#[macro_export]
macro_rules! tape_help {
    ($text:expr) => {
        #[used]
        #[unsafe(link_section = ".fos_help")]
        static FOS_HELP: [u8; $text.len()] = *$text;
    };
}

/// The zero-based interface chosen with `nic use N` (0 on a kernel older than ABI 4).
pub fn active_nic() -> usize {
    let table = api();
    if table.version >= 4 && (table.size as usize) >= core::mem::size_of::<TapeApi>() { (table.nic_active)() as usize } else { 0 }
}

/// Sets the machine's clock and RTC to `time` (local time). False if the kernel is older than ABI 6 or the date is not valid.
pub fn set_wall_clock(time: &WallClock) -> bool {
    let table = api();
    if table.version < 6 || (table.size as usize) < core::mem::size_of::<TapeApi>() {
        return false;
    }
    (table.wall_clock_set)(time) != 0
}

/// The console's text grid as (columns, rows); 80x25 when the kernel is older than ABI 8 or has no console.
pub fn console_size() -> (usize, usize) {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, console_size) + core::mem::size_of::<usize>();
    if table.version < 8 || (table.size as usize) < end {
        return (80, 25);
    }
    let packed = (table.console_size)();
    let (columns, rows) = ((packed >> 16) as usize, (packed & 0xffff) as usize);
    if columns == 0 || rows == 0 { (80, 25) } else { (columns, rows) }
}

/// Whether the kernel's table has the version-9 hardware timestamp calls.
fn has_timestamps() -> bool {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_send_stamped) + core::mem::size_of::<usize>();
    table.version >= 9 && (table.size as usize) >= end
}

/// Arms hardware timestamps on interface `nic`; false on an older kernel or a card without them.
pub fn nic_ts_enable(nic: usize) -> bool {
    has_timestamps() && (api().nic_ts_enable)(nic) != 0
}

/// The card's clock in nanoseconds (40 bits; 0 without timestamps).
pub fn nic_ts_clock(nic: usize) -> u64 {
    if has_timestamps() { (api().nic_ts_clock)(nic) } else { 0 }
}

/// The hardware stamp of the frame the last `nic_receive` on `nic` returned (0: none).
pub fn nic_ts_rx(nic: usize) -> u64 {
    if has_timestamps() { (api().nic_ts_rx)(nic) } else { 0 }
}

/// Whether the kernel's table has the version-10 clock steering calls.
fn has_steering() -> bool {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_ts_step) + core::mem::size_of::<usize>();
    table.version >= 10 && (table.size as usize) >= end
}

/// Turns transmit checksum offload on or off: with it on, `nic_send` has the card checksum IPv4 UDP frames whose checksum
/// field is 0 (and TCP frames). True if the card does it; false (a card without it, or a kernel older than version 14)
/// means the tape must compute checksums itself.
pub fn tx_offload(nic: usize, on: bool) -> bool {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_tx_offload) + core::mem::size_of::<usize>();
    table.version >= 14 && (table.size as usize) >= end && (table.nic_tx_offload)(nic, u32::from(on)) != 0
}

/// Number of values `nic_counters` can return, in the order the ABI documents (speed, duplex, missed, no-buffer, CRC,
/// symbol, XOFF received, XOFF sent, tape queue drops).
pub const NIC_COUNTERS: usize = 9;

/// Link speed and error counters of interface `nic` (see `TapeApi::nic_counters`); `None` on a kernel older than version
/// 15 or an unknown interface.
pub fn nic_counters(nic: usize) -> Option<[u64; NIC_COUNTERS]> {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_counters) + core::mem::size_of::<usize>();
    if table.version < 15 || (table.size as usize) < end {
        return None;
    }
    let mut values = [0u64; NIC_COUNTERS];
    ((table.nic_counters)(nic, values.as_mut_ptr(), NIC_COUNTERS) != 0).then_some(values)
}

/// Registers (`on`) or drops one VLAN id in the card's VLAN filter; the filter turns on with the first id. True if the
/// card did it; false on a card without one or a kernel older than version 13 (every tag then still arrives).
pub fn vlan_filter(nic: usize, vid: u16, on: bool) -> bool {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_vlan_filter) + core::mem::size_of::<usize>();
    table.version >= 13 && (table.size as usize) >= end && (table.nic_vlan_filter)(nic, u32::from(vid), u32::from(on)) != 0
}

/// Joins (`on`) or leaves one multicast group in the card's hash filter. True if the card did it; false means it has no
/// such filter (or the kernel is older than version 12) and the caller opens all multicast with `nic_set_multicast`.
pub fn multicast_group(nic: usize, mac: [u8; 6], on: bool) -> bool {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_multicast_join) + core::mem::size_of::<usize>();
    table.version >= 12 && (table.size as usize) >= end && (table.nic_multicast_join)(nic, mac.as_ptr(), u32::from(on)) != 0
}

/// Runs the card's clock `ppb` parts per billion fast (negative: slow); 0 restores the nominal rate. False if it cannot.
pub fn nic_ts_adjust(nic: usize, ppb: i32) -> bool {
    has_steering() && (api().nic_ts_adjust)(nic, ppb) != 0
}

/// Moves the card's clock by `delta_ns`. False if it cannot.
pub fn nic_ts_step(nic: usize, delta_ns: i64) -> bool {
    has_steering() && (api().nic_ts_step)(nic, delta_ns) != 0
}

/// Stamps every received frame on `nic` (after `nic_ts_enable`), so `nic_ts_rx` is valid for each one; false if it cannot.
pub fn nic_ts_all(nic: usize, on: bool) -> bool {
    let table = api();
    let end = core::mem::offset_of!(fosfor_abi::tape::TapeApi, nic_ts_all) + core::mem::size_of::<usize>();
    table.version >= 11 && (table.size as usize) >= end && (table.nic_ts_all)(nic, u32::from(on)) != 0
}

const RX_NICS: usize = 8;
/// Per interface: 0 not tried, 1 stamping every frame, 2 the card cannot.
static RX_STATE: [core::sync::atomic::AtomicU8; RX_NICS] = [const { core::sync::atomic::AtomicU8::new(0) }; RX_NICS];
/// The card clock (ns) and the tape clock (us) read together, so a card stamp can be moved onto the tape clock.
static RX_CARD_NS: [core::sync::atomic::AtomicU64; RX_NICS] = [const { core::sync::atomic::AtomicU64::new(0) }; RX_NICS];
static RX_LOCAL_US: [core::sync::atomic::AtomicU64; RX_NICS] = [const { core::sync::atomic::AtomicU64::new(0) }; RX_NICS];

/// `true` makes `rx_arrival_us` ignore the card's stamps (for comparing against them).
pub fn rx_software(on: bool) {
    for state in &RX_STATE {
        state.store(if on { 2 } else { 0 }, core::sync::atomic::Ordering::Relaxed);
    }
}

/// The arrival time, on the `micros()` clock, of the frame the last `nic_receive(nic, ...)` returned. With hardware
/// timestamps (the first call arms the card to stamp every frame) this is when the card saw it, free of the polling
/// loop's delay; without them it is `micros()` now. The card clock is tied to `micros()` again every 100 ms, so
/// the two crystals cannot drift apart by more than nanoseconds in between.
pub fn rx_arrival_us(nic: usize) -> u64 {
    use core::sync::atomic::Ordering::Relaxed;
    let now = micros();
    if nic >= RX_NICS {
        return now;
    }
    if RX_STATE[nic].load(Relaxed) == 0 {
        let armed = nic_ts_enable(nic) && nic_ts_all(nic, true);
        RX_STATE[nic].store(if armed { 1 } else { 2 }, Relaxed);
        RX_LOCAL_US[nic].store(0, Relaxed);
    }
    if RX_STATE[nic].load(Relaxed) != 1 {
        return now;
    }
    let stamp = nic_ts_rx(nic);
    if stamp == 0 {
        return now;
    }
    if now.wrapping_sub(RX_LOCAL_US[nic].load(Relaxed)) > 100_000 {
        let before = micros();
        let card = nic_ts_clock(nic);
        let after = micros();
        RX_CARD_NS[nic].store(card, Relaxed);
        RX_LOCAL_US[nic].store(before + (after - before) / 2, Relaxed);
    }
    const MASK: u64 = (1 << 40) - 1;
    let mut delta = (stamp.wrapping_sub(RX_CARD_NS[nic].load(Relaxed)) & MASK) as i64;
    if delta >= 1 << 39 {
        delta -= 1 << 40; // the frame came before the reading
    }
    ((RX_LOCAL_US[nic].load(Relaxed) as i64 * 1000 + delta) / 1000) as u64
}

/// Sends `frame`; returns (sent, hardware transmit stamp or 0).
pub fn nic_send_stamped(nic: usize, frame: &[u8]) -> (bool, u64) {
    let mut stamp = 0u64;
    let sent = if has_timestamps() {
        (api().nic_send_stamped)(nic, frame.as_ptr(), frame.len(), &mut stamp) != 0
    } else {
        (api().nic_send)(nic, frame.as_ptr(), frame.len()) != 0
    };
    (sent, stamp)
}

pub fn micros() -> u64 {
    (api().micros)()
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    // A tape has nowhere to unwind to: say what happened, then wait for the operator to
    // abort (the kernel's Escape handling is what ends a tape that will not return).
    println!("\ntape panic: {info}");
    loop {
        if (api().should_abort)() != 0 {
            // Cannot return from a panic; park. (The operator resets, or the kernel's
            // watchdog does, from M6.)
            idle();
        }
        idle();
    }
}
