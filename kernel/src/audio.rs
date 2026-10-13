//! The audio facade: one set of calls in front of the audio devices the kernel can drive.
//!
//! There are two kinds of device, USB audio class (`usb_audio.rs`, behind an xHCI or EHCI controller) and Intel HD Audio
//! (`hda.rs`, a PCI controller with a codec). Tapes know neither: they see `audio_info`, `audio_select`,
//! `audio_start`, `audio_read`, `audio_write`, `audio_stats` and `audio_stop` of the tape ABI, which the kernel
//! routes through this module to *the active device*.
//!
//! Which device is active: the one the user picked (`audio device N`), else automatically the USB audio device if
//! there is one (so a machine that had one before HD Audio was supported behaves as it did), else the HD Audio
//! controller with the best playback and capture paths. A tape cannot choose; the operator does, before running it.
//!
//! Both backends move PCM through the same `PcmRing`: a single-producer single-consumer byte FIFO that the
//! interrupt handler and the foreground share, so `audio_write` returns how much fitted and `audio_read` what has
//! arrived, whichever device is behind them.

use crate::{hda, usb_audio as usb};
pub use crate::usb_audio::PcmRing;
use fosfor_abi::tape::{AudioDevice, AudioInfo, AudioStats, AUDIO_KIND_HDA, AUDIO_KIND_USB, AUDIO_NO_DEVICE};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Usb,
    Hda(usize),
}

/// The device the user picked; `None` is automatic.
static mut CHOSEN: Option<Backend> = None;
/// While a tape has chosen a device (`audio_use_device`), what the operator had chosen before, to put back when the tape ends.
/// `None` = no tape has chosen anything.
static mut OPERATOR_CHOICE: Option<Option<Backend>> = None;

fn available(backend: Backend) -> bool {
    match backend {
        Backend::Usb => usb::present(),
        Backend::Hda(index) => hda::present(index),
    }
}

/// Every device that can be used, in the order `audio device` numbers them: USB audio first, then each HD Audio
/// controller that has a playback or capture path.
fn devices() -> ([Backend; 1 + hda::MAX_CONTROLLERS], usize) {
    let mut list = [Backend::Usb; 1 + hda::MAX_CONTROLLERS];
    let mut count = 0;
    if usb::present() {
        list[count] = Backend::Usb;
        count += 1;
    }
    for index in 0..hda::count() {
        if hda::present(index) {
            list[count] = Backend::Hda(index);
            count += 1;
        }
    }
    (list, count)
}

fn automatic() -> Option<Backend> {
    if usb::present() {
        return Some(Backend::Usb);
    }
    (0..hda::count()).filter(|&index| hda::present(index)).max_by_key(|&index| (hda::rank(index), usize::MAX - index)).map(Backend::Hda)
}

/// The device the next call goes to.
fn active() -> Option<Backend> {
    match unsafe { *core::ptr::addr_of!(CHOSEN) } {
        Some(backend) if available(backend) => Some(backend),
        _ => automatic(),
    }
}

/// `audio device N|auto`: picks the device (1-based, as `audio` lists them). Anything running is stopped first.
pub fn choose(number: Option<usize>) -> Result<(), &'static str> {
    let backend = match number {
        None => None,
        Some(number) => {
            let (list, count) = devices();
            if number == 0 || number > count {
                return Err("no such device (`audio` lists them)");
            }
            Some(list[number - 1])
        }
    };
    stop();
    unsafe { *core::ptr::addr_of_mut!(CHOSEN) = backend };
    Ok(())
}

/// How many devices `audio` lists (and `audio_devices` counts).
pub fn device_count() -> usize {
    devices().1
}

/// The `AudioDevice` for device `number` (1-based), for the tape call `audio_devices`.
pub fn device_info(number: usize) -> Option<AudioDevice> {
    let (list, count) = devices();
    if number == 0 || number > count {
        return None;
    }
    let backend = list[number - 1];
    let active = active() == Some(backend);
    Some(match backend {
        Backend::Usb => AudioDevice { kind: AUDIO_KIND_USB, active: active as u32, ..AudioDevice::default() },
        Backend::Hda(index) => {
            let (bus, device, function, vendor, id) = hda::label(index);
            AudioDevice {
                kind: AUDIO_KIND_HDA,
                active: active as u32,
                pci_location: (u32::from(bus) << 16) | (u32::from(device) << 8) | u32::from(function),
                vendor,
                id,
            }
        }
    })
}

/// The tape call `audio_use_device`: like `audio device N|auto`, but remembered as the tape's own choice, so
/// `stop_for_tape` can put the operator's back. 0 for automatic.
pub fn use_device_for_tape(number: u32) -> i32 {
    let before = unsafe { *core::ptr::addr_of!(CHOSEN) };
    match choose(if number == 0 { None } else { Some(number as usize) }) {
        Ok(()) => {
            // Only the first choice of a tape records what to restore: a second call must not turn the tape's own pick into "the operator's".
            let saved = unsafe { &mut *core::ptr::addr_of_mut!(OPERATOR_CHOICE) };
            if saved.is_none() {
                *saved = Some(before);
            }
            0
        }
        Err(_) => AUDIO_NO_DEVICE,
    }
}

/// The ordinary starter for the shell and tapes. The error strings are the ones `start_for_tape` maps to codes.
pub fn start(rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    match active() {
        Some(Backend::Usb) => usb::start(rate, capture, playback),
        Some(Backend::Hda(index)) => hda::start(index, rate, capture, playback),
        None => Err("no audio device"),
    }
}

/// Stops every running stream on every device.
pub fn stop() {
    usb::stop();
    for index in 0..hda::count() {
        hda::stop(index);
    }
}

pub fn read(out: &mut [u8]) -> usize {
    match active() {
        Some(Backend::Usb) => usb::read(out),
        Some(Backend::Hda(index)) => hda::read(index, out),
        None => 0,
    }
}

pub fn write(data: &[u8]) -> usize {
    match active() {
        Some(Backend::Usb) => usb::write(data),
        Some(Backend::Hda(index)) => hda::write(index, data),
        None => 0,
    }
}

pub fn playback_space() -> usize {
    match active() {
        Some(Backend::Usb) => usb::playback_space(),
        Some(Backend::Hda(index)) => hda::playback_space(index),
        None => 0,
    }
}

/// (channels, bytes per sample slot) of the running capture / playback stream.
pub fn capture_format() -> Option<(u8, u8)> {
    match active() {
        Some(Backend::Usb) => usb::capture_format(),
        Some(Backend::Hda(index)) => hda::format(index, true),
        None => None,
    }
}

pub fn playback_format() -> Option<(u8, u8)> {
    match active() {
        Some(Backend::Usb) => usb::playback_format(),
        Some(Backend::Hda(index)) => hda::format(index, false),
        None => None,
    }
}

/// What the foreground must do to keep a polled device moving (an HD Audio controller without working interrupts;
/// the USB side is moved by `usb_hid::poll`, which the callers already do).
pub fn poll() {
    usb::ehci_poll();
    hda::poll();
}

pub fn select(capture: bool, mode: u32) -> Result<(), &'static str> {
    match active() {
        Some(Backend::Usb) => usb::select(capture, mode),
        Some(Backend::Hda(index)) => hda::select(index, capture, mode),
        None => Err("no audio device"),
    }
}

pub fn print_modes() {
    match active() {
        Some(Backend::Usb) => usb::print_modes(),
        Some(Backend::Hda(index)) => hda::print_modes(index),
        None => crate::println!("No audio device."),
    }
}

/// `audio` with no arguments: the devices (when there is a choice) and the active one's state.
pub fn status() {
    let (list, count) = devices();
    let Some(current) = active() else { return crate::println!("No audio device.") };
    if count > 1 {
        crate::println!("Audio devices (`audio device N` picks one, `audio device auto` lets the kernel):");
        for (position, backend) in list[..count].iter().enumerate() {
            crate::print(format_args!("  {}{}: ", if *backend == current { '*' } else { ' ' }, position + 1));
            match backend {
                Backend::Usb => crate::println!("USB audio"),
                Backend::Hda(index) => {
                    let (bus, device, function, vendor, id) = hda::label(*index);
                    crate::println!("HD Audio {bus:02x}:{device:02x}.{function} ({vendor:04x}:{id:04x})");
                }
            }
        }
    }
    match current {
        Backend::Usb => usb::status(),
        Backend::Hda(index) => hda::status(index),
    }
}

pub fn fill_info(info: &mut AudioInfo) -> bool {
    match active() {
        Some(Backend::Usb) => usb::fill_info(info),
        Some(Backend::Hda(index)) => hda::fill_info(index, info),
        None => false,
    }
}

pub fn fill_stats(stats: &mut AudioStats) {
    match active() {
        Some(Backend::Usb) => usb::fill_stats(stats),
        Some(Backend::Hda(index)) => hda::fill_stats(index, stats),
        None => *stats = AudioStats::default(),
    }
}

pub fn buffer_info() -> (u64, u64) {
    match active() {
        Some(Backend::Usb) => usb::buffer_info(),
        Some(Backend::Hda(index)) => hda::buffer_info(index),
        None => (0, 0),
    }
}

/// The tape ABI's `AUDIO_*` code for a failed start (0 for success).
fn code(result: Result<(), &'static str>) -> i32 {
    use fosfor_abi::tape::*;
    match result {
        Ok(()) => 0,
        Err("no USB audio device") | Err("no HD Audio device") | Err("no audio device") => AUDIO_NO_DEVICE,
        Err("already running") => AUDIO_BUSY,
        Err("the device does not offer that sample rate") => AUDIO_BAD_RATE,
        Err("the device has no capture stream") | Err("the device has no playback stream") => AUDIO_NO_DIRECTION,
        Err(_) => AUDIO_FAILED,
    }
}

pub fn start_for_tape(rate: u32, capture: bool, playback: bool) -> i32 {
    code(start(rate, capture, playback))
}

pub fn select_for_tape(capture: bool, mode: u32) -> i32 {
    code(select(capture, mode).map_err(|reason| if reason.starts_with("no such mode") { "mode" } else { reason }))
}

/// A tape that returns (or was aborted) must not leave a stream running, or its big capture ring allocated.
pub fn stop_for_tape() {
    usb::set_capture_ring(0);
    stop();
    // A device the tape picked is not the operator's choice: put the operator's back.
    if let Some(before) = unsafe { (*core::ptr::addr_of_mut!(OPERATOR_CHOICE)).take() } {
        unsafe { *core::ptr::addr_of_mut!(CHOSEN) = before };
    }
}
