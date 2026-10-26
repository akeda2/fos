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
//! Playback can go to a different device than capture (`audio output N|same`, the tape call `audio_use_output`): capture,
//! `audio_read` and the capture format follow the input device above, `audio_write` and the playback format follow the output.
//! `audio_start` then starts one direction on each device. Two devices of the same kind work for HD Audio; USB audio is a
//! up to `usb::UNITS` instances, so USB in plus USB out, or USB with HD Audio, work too (two USB devices on one EHCI controller
//! do not: it has one isochronous schedule).
//!
//! Both backends move PCM through the same `PcmRing`: a single-producer single-consumer byte FIFO that the
//! interrupt handler and the foreground share, so `audio_write` returns how much fitted and `audio_read` what has
//! arrived, whichever device is behind them.

use crate::{hda, loopback, usb_audio as usb};
#[cfg(not(target_arch = "aarch64"))]
pub use crate::usb_audio::PcmRing;
#[cfg(target_arch = "aarch64")]
use crate::i2s_pi as i2s;
#[cfg(not(target_arch = "aarch64"))]
use crate::i2s_none as i2s;
use fosfor_abi::tape::{AudioDevice, AudioInfo, AudioStats, AUDIO_KIND_HDA, AUDIO_KIND_I2S, AUDIO_KIND_LOOP, AUDIO_KIND_USB, AUDIO_BUSY, AUDIO_NO_DEVICE};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Usb(usize),
    Hda(usize),
    /// The Pi's I2S audio HAT (`i2s_pi.rs`), offered only once switched on.
    I2s,
    /// A virtual loopback device (`loopback.rs`): what one tape plays into it another captures.
    Loop(usize),
}

/// The device choices of one context: 0 is the shell or foreground tape (the operator's `audio device`), `n` is service
/// `n - 1`. A service's explicit choice is also its claim: nothing else may pick that device while it runs.
#[derive(Clone, Copy)]
struct Choice {
    /// The device picked; `None` is automatic.
    chosen: Option<Backend>,
    /// While a tape has chosen a device (`audio_use_device`), what was chosen before, to put back when the tape ends.
    /// `None` = the tape has chosen nothing.
    saved_chosen: Option<Option<Backend>>,
    /// A separate playback device (`audio output`); `None` = playback follows the active device.
    output: Option<Backend>,
    /// What was chosen as the output before a tape did (`audio_use_output`).
    saved_output: Option<Option<Backend>>,
}

impl Choice {
    const NONE: Choice = Choice { chosen: None, saved_chosen: None, output: None, saved_output: None };
}

static mut CHOICES: [Choice; 1 + crate::tasks::MAX_SERVICES] = [Choice::NONE; 1 + crate::tasks::MAX_SERVICES];

const CLAIMED: &str = "that audio device belongs to a service";

fn choices() -> &'static mut [Choice; 1 + crate::tasks::MAX_SERVICES] {
    unsafe { &mut *core::ptr::addr_of_mut!(CHOICES) }
}

/// The choices of the running context.
fn mine() -> &'static mut Choice {
    &mut choices()[crate::tasks::context()]
}

/// The service (1-based context number) that claimed `backend`, other than the running context. A loopback is never
/// claimed whole: it has two ends, and `loopback::start` gives each to one context at a time.
fn claimed_by_other(backend: Backend) -> Option<usize> {
    if matches!(backend, Backend::Loop(_)) {
        return None;
    }
    let me = crate::tasks::context();
    (1..choices().len()).find(|&context| context != me && crate::tasks::context_live(context) && (choices()[context].chosen == Some(backend) || choices()[context].output == Some(backend)))
}

fn available(backend: Backend) -> bool {
    match backend {
        Backend::Usb(unit) => usb::present(unit),
        Backend::Hda(index) => hda::present(index),
        Backend::I2s => i2s::present(),
        Backend::Loop(index) => loopback::present(index),
    }
}

/// Every device that can be used, in the order `audio device` numbers them: USB audio first (by unit), then each HD Audio
/// controller that has a playback or capture path.
fn devices() -> ([Backend; usb::UNITS + hda::MAX_CONTROLLERS + 1 + loopback::COUNT], usize) {
    let mut list = [Backend::Usb(0); usb::UNITS + hda::MAX_CONTROLLERS + 1 + loopback::COUNT];
    let mut count = 0;
    for unit in 0..usb::UNITS {
        if usb::present(unit) {
            list[count] = Backend::Usb(unit);
            count += 1;
        }
    }
    for index in 0..hda::count() {
        if hda::present(index) {
            list[count] = Backend::Hda(index);
            count += 1;
        }
    }
    if i2s::present() {
        list[count] = Backend::I2s;
        count += 1;
    }
    // The loopbacks come last, so the numbers of real devices do not change; they are never picked automatically.
    for index in 0..loopback::COUNT {
        list[count] = Backend::Loop(index);
        count += 1;
    }
    (list, count)
}

fn automatic() -> Option<Backend> {
    let free = |backend: Backend| claimed_by_other(backend).is_none();
    if let Some(unit) = (0..usb::UNITS).find(|&unit| usb::present(unit) && free(Backend::Usb(unit))) {
        return Some(Backend::Usb(unit));
    }
    (0..hda::count()).filter(|&index| hda::present(index) && free(Backend::Hda(index))).max_by_key(|&index| (hda::rank(index), usize::MAX - index)).map(Backend::Hda).or((i2s::present() && free(Backend::I2s)).then_some(Backend::I2s))
}

/// The device the next call goes to.
fn active() -> Option<Backend> {
    match mine().chosen {
        Some(backend) if available(backend) => Some(backend),
        _ => automatic(),
    }
}

/// The device playback goes to: the separate output when one is chosen and present, else the active device.
fn output() -> Option<Backend> {
    match mine().output {
        Some(backend) if available(backend) => Some(backend),
        _ => active(),
    }
}

fn split() -> bool {
    output() != active()
}

fn backend_number(number: Option<usize>) -> Result<Option<Backend>, &'static str> {
    match number {
        None => Ok(None),
        Some(number) => {
            let (list, count) = devices();
            if number == 0 || number > count {
                return Err("no such device (`audio` lists them)");
            }
            Ok(Some(list[number - 1]))
        }
    }
}

/// `audio output N|same`: playback goes to device N (1-based, as `audio` lists them); `same` follows the input device.
pub fn choose_output(number: Option<usize>) -> Result<(), &'static str> {
    let backend = backend_number(number)?;
    if backend.is_some_and(|backend| claimed_by_other(backend).is_some()) {
        return Err(CLAIMED);
    }
    stop_mine();
    mine().output = backend;
    Ok(())
}

/// The tape call `audio_use_output`: like `audio output N|same` (0 = same), undone when the tape ends.
pub fn use_output_for_tape(number: u32) -> i32 {
    let before = mine().output;
    match choose_output(if number == 0 { None } else { Some(number as usize) }) {
        Ok(()) => {
            let saved = &mut mine().saved_output;
            if saved.is_none() {
                *saved = Some(before);
            }
            0
        }
        Err(CLAIMED) => AUDIO_BUSY,
        Err(_) => AUDIO_NO_DEVICE,
    }
}

/// `audio device N|auto`: picks the device (1-based, as `audio` lists them). Anything running is stopped first.
pub fn choose(number: Option<usize>) -> Result<(), &'static str> {
    let backend = backend_number(number)?;
    if backend.is_some_and(|backend| claimed_by_other(backend).is_some()) {
        return Err(CLAIMED);
    }
    stop_mine();
    mine().chosen = backend;
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
        Backend::Usb(_) => AudioDevice { kind: AUDIO_KIND_USB, active: active as u32, ..AudioDevice::default() },
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
        Backend::I2s => AudioDevice { kind: AUDIO_KIND_I2S, active: active as u32, ..AudioDevice::default() },
        Backend::Loop(index) => AudioDevice { kind: AUDIO_KIND_LOOP, active: active as u32, id: index as u16, ..AudioDevice::default() },
    })
}

/// The tape call `audio_use_device`: like `audio device N|auto`, but remembered as the tape's own choice, so
/// `stop_for_tape` can put the operator's back. 0 for automatic.
pub fn use_device_for_tape(number: u32) -> i32 {
    let before = mine().chosen;
    match choose(if number == 0 { None } else { Some(number as usize) }) {
        Ok(()) => {
            // Only the first choice of a tape records what to restore: a second call must not turn the tape's own pick into "the operator's".
            let saved = &mut mine().saved_chosen;
            if saved.is_none() {
                *saved = Some(before);
            }
            0
        }
        Err(CLAIMED) => AUDIO_BUSY,
        Err(_) => AUDIO_NO_DEVICE,
    }
}

/// The ordinary starter for the shell and tapes. The error strings are the ones `start_for_tape` maps to codes.
pub fn start(rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    if !split() {
        return start_on(active(), rate, capture, playback);
    }
    if capture {
        start_on(active(), rate, true, false)?;
    }
    if playback {
        if let Err(reason) = start_on(output(), rate, false, true) {
            stop_mine();
            return Err(reason);
        }
    }
    Ok(())
}

fn start_on(backend: Option<Backend>, rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    match backend {
        Some(Backend::Usb(unit)) => usb::start(unit, rate, capture, playback),
        Some(Backend::Hda(index)) => hda::start(index, rate, capture, playback),
        Some(Backend::I2s) => i2s::start(rate, capture, playback),
        Some(Backend::Loop(index)) => loopback::start(index, rate, capture, playback),
        None => Err("no audio device"),
    }
}

/// Stops a device's streams; a loopback has two ends and only the ones named stop.
fn stop_backend(backend: Backend, capture: bool, playback: bool) {
    match backend {
        Backend::Usb(unit) => usb::stop(unit),
        Backend::Hda(index) => hda::stop(index),
        Backend::I2s => i2s::stop(),
        Backend::Loop(index) => loopback::stop(index, capture, playback),
    }
}

/// Stops the streams on the devices the running context uses (its input and its output), and nobody else's: a service's
/// stream goes on when the foreground tape ends.
pub fn stop_mine() {
    let input = active();
    let out = output();
    if let Some(backend) = input {
        stop_backend(backend, true, out == input);
    }
    if let Some(backend) = out.filter(|&backend| Some(backend) != input) {
        stop_backend(backend, false, true);
    }
}

pub fn read(out: &mut [u8]) -> usize {
    match active() {
        Some(Backend::Usb(unit)) => usb::read(unit, out),
        Some(Backend::Hda(index)) => hda::read(index, out),
        Some(Backend::I2s) => i2s::read(out),
        Some(Backend::Loop(index)) => loopback::read(index, out),
        None => 0,
    }
}

pub fn write(data: &[u8]) -> usize {
    match output() {
        Some(Backend::Usb(unit)) => usb::write(unit, data),
        Some(Backend::Hda(index)) => hda::write(index, data),
        Some(Backend::I2s) => i2s::write(data),
        Some(Backend::Loop(index)) => loopback::write(index, data),
        None => 0,
    }
}

pub fn playback_space() -> usize {
    match output() {
        Some(Backend::Usb(unit)) => usb::playback_space(unit),
        Some(Backend::Hda(index)) => hda::playback_space(index),
        Some(Backend::I2s) => i2s::playback_space(),
        Some(Backend::Loop(index)) => loopback::playback_space(index),
        None => 0,
    }
}

/// (channels, bytes per sample slot) of the running capture / playback stream.
pub fn capture_format() -> Option<(u8, u8)> {
    match active() {
        Some(Backend::Usb(unit)) => usb::capture_format(unit),
        Some(Backend::Hda(index)) => hda::format(index, true),
        Some(Backend::I2s) => i2s::format(true),
        Some(Backend::Loop(index)) => loopback::format(index, true),
        None => None,
    }
}

pub fn playback_format() -> Option<(u8, u8)> {
    match output() {
        Some(Backend::Usb(unit)) => usb::playback_format(unit),
        Some(Backend::Hda(index)) => hda::format(index, false),
        Some(Backend::I2s) => i2s::format(false),
        Some(Backend::Loop(index)) => loopback::format(index, false),
        None => None,
    }
}

/// Whether the running capture / playback stream carries 32-bit IEEE floats instead of integers (USB only; HD Audio is PCM).
pub fn capture_float() -> bool {
    match active() {
        Some(Backend::Usb(unit)) => usb::capture_float(unit),
        Some(Backend::Loop(_)) => true,
        _ => false,
    }
}

pub fn playback_float() -> bool {
    match output() {
        Some(Backend::Usb(unit)) => usb::playback_float(unit),
        Some(Backend::Loop(_)) => true,
        _ => false,
    }
}

/// Tape ABI `audio_float`: the float flags of the streams a tape sees (the running or default mode of each direction).
pub extern "C" fn api_audio_float() -> u32 {
    let capture = match active() {
        Some(Backend::Usb(unit)) => usb::capture_float_default(unit),
        Some(Backend::Loop(_)) => true,
        _ => false,
    };
    let playback = match output() {
        Some(Backend::Usb(unit)) => usb::playback_float_default(unit),
        Some(Backend::Loop(_)) => true,
        _ => false,
    };
    u32::from(capture) | u32::from(playback) << 1
}

/// What the foreground must do to keep a polled device moving (an HD Audio controller without working interrupts;
/// the USB side is moved by `usb_hid::poll`, which the callers already do).
pub fn poll() {
    usb::ehci_poll();
    hda::poll();
    i2s::poll();
}

pub fn select(capture: bool, mode: u32) -> Result<(), &'static str> {
    match if capture { active() } else { output() } {
        Some(Backend::Usb(unit)) => usb::select(unit, capture, mode),
        Some(Backend::Hda(index)) => hda::select(index, capture, mode),
        Some(Backend::I2s) => i2s::select(capture, mode),
        Some(Backend::Loop(_)) => loopback::select(capture, mode),
        None => Err("no audio device"),
    }
}

pub fn print_modes() {
    match active() {
        Some(Backend::Usb(unit)) => usb::print_modes(unit),
        Some(Backend::Hda(index)) => hda::print_modes(index),
        Some(Backend::I2s) => i2s::print_modes(),
        Some(Backend::Loop(_)) => loopback::print_modes(),
        None => crate::println!("No audio device."),
    }
}

/// `audio` with no arguments: the devices (when there is a choice) and the active one's state.
pub fn status() {
    let (list, count) = devices();
    let current = active();
    if current.is_none() {
        crate::println!("No audio hardware; only the loopbacks are available (`audio device loopN` picks one).");
    }
    if count > 1 || current.is_none() {
        crate::println!("Audio devices (`audio device N` picks one, `audio device auto` lets the kernel):");
        for (position, backend) in list[..count].iter().enumerate() {
            crate::print(format_args!("  {}{}: ", if Some(*backend) == current { '*' } else { ' ' }, position + 1));
            match backend {
                Backend::Usb(unit) => crate::println!("{}", usb::label(*unit)),
                Backend::Hda(index) => {
                    let (bus, device, function, vendor, id) = hda::label(*index);
                    crate::println!("HD Audio {bus:02x}:{device:02x}.{function} ({vendor:04x}:{id:04x})");
                }
                Backend::I2s => crate::println!("{}", i2s::label()),
                Backend::Loop(index) => crate::println!("Loopback {index} (virtual; `loop{index}` names it)"),
            }
        }
    }
    if split() {
        if let Some(out) = output() {
            let position = list[..count].iter().position(|backend| *backend == out).map_or(0, |at| at + 1);
            crate::println!("Playback goes to device {position} (`audio output same` follows the input).");
        }
    }
    match current {
        Some(Backend::Usb(unit)) => usb::status(unit),
        Some(Backend::Hda(index)) => hda::status(index),
        Some(Backend::I2s) => i2s::status(),
        Some(Backend::Loop(index)) => loopback::status(index),
        None => {}
    }
}

fn info_of(backend: Option<Backend>, info: &mut AudioInfo) -> bool {
    match backend {
        Some(Backend::Usb(unit)) => usb::fill_info(unit, info),
        Some(Backend::Hda(index)) => hda::fill_info(index, info),
        Some(Backend::I2s) => i2s::fill_info(info),
        Some(Backend::Loop(index)) => {
            loopback::fill_info(index, info);
            true
        }
        None => false,
    }
}

/// With a separate output the capture side comes from the input device, the playback side from the output device, and
/// the rates are the ones both offer (one rate serves both streams).
pub fn fill_info(info: &mut AudioInfo) -> bool {
    let found = info_of(active(), info);
    if !split() {
        return found;
    }
    let mut out = AudioInfo::default();
    if !info_of(output(), &mut out) {
        return found;
    }
    info.playback_channels = out.playback_channels;
    info.playback_slot_bytes = out.playback_slot_bytes;
    info.playback_bits = out.playback_bits;
    info.playback_rate = out.playback_rate;
    info.playback_space = out.playback_space;
    if !found {
        info.capture_channels = 0;
        info.capture_slot_bytes = 0;
        info.capture_bits = 0;
        info.rate_count = out.rate_count;
        info.rates = out.rates;
        return true;
    }
    let mut common = [0u32; 8];
    let mut count = 0;
    for &rate in &info.rates[..(info.rate_count as usize).min(8)] {
        if out.rates[..(out.rate_count as usize).min(8)].contains(&rate) {
            common[count] = rate;
            count += 1;
        }
    }
    info.rates = common;
    info.rate_count = count as u32;
    true
}

pub fn fill_stats(stats: &mut AudioStats) {
    match active() {
        Some(Backend::Usb(unit)) => usb::fill_stats(unit, stats),
        Some(Backend::Hda(index)) => hda::fill_stats(index, stats),
        Some(Backend::I2s) => i2s::fill_stats(stats),
        Some(Backend::Loop(index)) => loopback::fill_stats(index, stats),
        None => *stats = AudioStats::default(),
    }
    if split() {
        let mut out = AudioStats::default();
        match output() {
            Some(Backend::Usb(unit)) => usb::fill_stats(unit, &mut out),
            Some(Backend::Hda(index)) => hda::fill_stats(index, &mut out),
            Some(Backend::I2s) => i2s::fill_stats(&mut out),
            Some(Backend::Loop(index)) => loopback::fill_stats(index, &mut out),
            None => {}
        }
        stats.playback_packets = out.playback_packets;
        stats.playback_errors = out.playback_errors;
        stats.playback_underruns = out.playback_underruns;
    }
}

pub fn buffer_info() -> (u64, u64) {
    match active() {
        Some(Backend::Usb(unit)) => usb::buffer_info(unit),
        Some(Backend::Hda(index)) => hda::buffer_info(index),
        Some(Backend::I2s) => i2s::buffer_info(),
        Some(Backend::Loop(index)) => loopback::buffer_info(index),
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
    stop_mine();
    // A device the tape picked is not the operator's choice: put the operator's back.
    if let Some(before) = mine().saved_chosen.take() {
        mine().chosen = before;
    }
    if let Some(before) = mine().saved_output.take() {
        mine().output = before;
    }
}

/// `run NAME device=N outdev=M`: service context `context` claims its devices (1-based, as `audio` lists them) before it
/// starts. Refuses a device that does not exist or that another service holds.
pub fn claim(context: usize, input: Option<usize>, output: Option<usize>) -> Result<(), &'static str> {
    let input = backend_number(input)?;
    let output = backend_number(output)?;
    for backend in [input, output].into_iter().flatten() {
        if let Some(owner) = claimed_by_other(backend) {
            crate::println!("run: audio device {} is owned by service {}.", number_of(backend), owner);
            return Err(CLAIMED);
        }
    }
    choices()[context] = Choice { chosen: input, output, ..Choice::NONE };
    Ok(())
}

/// The service ended (still running as that context): stop its streams and drop its claims.
pub fn release_service() {
    stop_mine();
    *mine() = Choice::NONE;
}

/// A service that never started gives its claims back.
pub fn unclaim(context: usize) {
    choices()[context] = Choice::NONE;
}

/// A device named by a person: a number as `audio` lists it, or `loopN` for a loopback.
pub fn parse_device(text: &str) -> Option<usize> {
    match text.strip_prefix("loop") {
        Some(index) => {
            let index = index.parse::<usize>().ok().filter(|&index| index < loopback::COUNT)?;
            Some(number_of(Backend::Loop(index)))
        }
        None => text.parse().ok(),
    }
}

fn number_of(backend: Backend) -> usize {
    let (list, count) = devices();
    list[..count].iter().position(|&other| other == backend).map_or(0, |index| index + 1)
}

/// What service `context` holds, for `ps`: the numbers of its input and output devices (0 = automatic / follows input).
pub fn claims_of(context: usize) -> (usize, usize) {
    let choice = &choices()[context];
    (choice.chosen.map_or(0, number_of), choice.output.map_or(0, number_of))
}
