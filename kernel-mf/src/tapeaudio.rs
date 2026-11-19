//! Audio calls for tapes (`audio_*`), on the shared `kernel_core::audio` facade over USB audio and Intel HD Audio.
//!
//! The drivers are unlocked and share their rings with interrupt handlers that run on the boot CPU, so a tape that touches audio
//! is pinned to CPU 0 (`sched::pin_current`), as every tape is on the embedded kernel, and each call holds the USB lock (an
//! interrupt-free poll of the xHCI rings is what `audio_read`/`audio_write` also do). A tape's memory is never handed to a
//! device: PCM goes through kernel bounce buffers of up to 64 KiB per call (a short count is a normal answer, as on the
//! embedded kernel). The stream belongs to the process that started it, until it ends or calls `audio_stop`.

use core::sync::atomic::{AtomicU64, Ordering};
use fosfor_abi::tape::{AudioBuffer, AudioDevice, AudioInfo, AudioStats, AUDIO_BUSY};
use kernel_core::{audio, usb_audio};

use crate::{process, sched, usb};

const CHUNK: usize = 64 * 1024;
static OWNER: AtomicU64 = AtomicU64::new(0);

fn pin() -> usb::UsbGuard {
    sched::pin_current(0);
    usb::lock()
}

fn put<T: Copy>(address: u64, value: &T) -> bool {
    let bytes = unsafe { core::slice::from_raw_parts((value as *const T).cast::<u8>(), core::mem::size_of::<T>()) }; // kernel-memory
    process::copy_to_user(address, bytes)
}

/// The pid that holds the audio stream, 0 when nobody does (`top`).
pub fn owner() -> u64 {
    OWNER.load(Ordering::Acquire)
}

pub fn info(out: u64) -> u64 {
    let _usb = pin();
    let mut info = AudioInfo::default();
    let found = audio::fill_info(&mut info);
    u64::from(found && put(out, &info))
}

pub fn start(rate: u64, capture: u64, playback: u64) -> i64 {
    let _usb = pin();
    let pid = sched::current_pid();
    if OWNER.compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire).is_err() && OWNER.load(Ordering::Acquire) != pid {
        return i64::from(AUDIO_BUSY);
    }
    // The drivers build a stream with interrupts enabled (control transfers wait on timer ticks), as the shell does; a
    // system call arrives with them off, which hung the first stream start of a tape on EHCI audio.
    let was_on = crate::cpu::interrupts_save_and_disable();
    crate::cpu::interrupts_enable();
    let result = audio::start_for_tape(rate as u32, capture != 0, playback != 0);
    crate::cpu::interrupts_restore(was_on);
    if result != 0 {
        OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).ok();
    }
    i64::from(result)
}

pub fn stop() {
    let _usb = pin();
    let pid = sched::current_pid();
    if OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        audio::stop_for_tape();
    }
}

/// A process ended: stop its stream and give back the devices it picked.
pub fn process_ended(pid: u64) {
    if OWNER.compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_ok() {
        let _usb = usb::lock();
        audio::stop_for_tape();
    }
}

pub fn read(out: u64, length: u64) -> u64 {
    let _usb = pin();
    kernel_core::usb_class::poll();
    let mut buffer = alloc::vec![0u8; (length as usize).min(CHUNK)];
    let got = audio::read(&mut buffer);
    if got > 0 && !process::copy_to_user(out, &buffer[..got]) {
        return 0;
    }
    got as u64
}

pub fn write(data: u64, length: u64) -> u64 {
    let _usb = pin();
    kernel_core::usb_class::poll();
    let Some(bytes) = process::user_bytes_up_to(data, length.min(CHUNK as u64), CHUNK as u64) else { return 0 };
    audio::write(&bytes) as u64
}

pub fn stats(out: u64) {
    let _usb = pin();
    let mut stats = AudioStats::default();
    audio::fill_stats(&mut stats);
    put(out, &stats);
}

pub fn select(direction: u64, mode: u64) -> i64 {
    let _usb = pin();
    i64::from(audio::select_for_tape(direction == 0, mode as u32))
}

pub fn buffer(out: u64) {
    let _usb = pin();
    let (ring_bytes, available_bytes) = audio::buffer_info();
    put(out, &AudioBuffer { ring_bytes, available_bytes });
}

pub fn set_capture_ring(bytes: u64) {
    usb_audio::set_capture_ring(bytes);
}

/// `audio_devices(number, out)`: the count; `out` (when non-zero) gets device `number`.
pub fn devices(number: u64, out: u64) -> u64 {
    let _usb = pin();
    if out != 0 {
        if let Some(device) = audio::device_info(number as usize) {
            let device: AudioDevice = device;
            put(out, &device);
        }
    }
    audio::device_count() as u64
}

pub fn use_device(number: u64) -> i64 {
    let _usb = pin();
    i64::from(audio::use_device_for_tape(number as u32))
}

pub fn use_output(number: u64) -> i64 {
    let _usb = pin();
    i64::from(audio::use_output_for_tape(number as u32))
}

pub fn float() -> u64 {
    let _usb = pin();
    u64::from(audio::api_audio_float())
}

pub fn state() -> i32 {
    let _usb = pin();
    audio::state()
}

// The mixer calls (ABI 41) on the active device's mixer (`audio::mixer`). A driver's mixer may make USB control transfers,
// hence the pin and the USB lock like every audio call. Names and meters go through kernel buffers of bounded size.

/// The longest name copied back: names are short labels.
const NAME_MAX: usize = 64;
/// The most meters one call returns (a large interface has about 30 inputs, 30 playback channels and 30 outputs).
const METERS_MAX: usize = 256;

fn mixer_signed(value: i32) -> i64 {
    i64::from(value)
}

pub fn mixer_info(out: u64) -> i32 {
    let _usb = pin();
    let mut info = fosfor_abi::tape::MixerInfo::default();
    match audio::mixer() {
        Some(mixer) if mixer.info(&mut info) => if put(out, &info) { 0 } else { fosfor_abi::tape::MIXER_BAD },
        _ => fosfor_abi::tape::MIXER_NONE,
    }
}

fn copy_name(buffer: u64, capacity: u64, fill: impl FnOnce(&mut [u8]) -> Option<usize>) -> i64 {
    let mut name = [0u8; NAME_MAX];
    let length = (capacity as usize).min(NAME_MAX);
    match fill(&mut name[..length]) {
        Some(count) if process::copy_to_user(buffer, &name[..count]) => count as i64,
        _ => mixer_signed(fosfor_abi::tape::MIXER_BAD),
    }
}

pub fn mixer_name(group: u64, index: u64, buffer: u64, capacity: u64) -> i64 {
    let _usb = pin();
    let Some(mixer) = audio::mixer() else { return mixer_signed(fosfor_abi::tape::MIXER_NONE) };
    copy_name(buffer, capacity, |out| mixer.name(group as u32, index as u32, out))
}

pub fn mixer_get(output: u64, source: u64) -> i64 {
    let _usb = pin();
    audio::mixer().map_or(mixer_signed(fosfor_abi::tape::MIXER_NONE), |mixer| mixer.get(output as u32, source as u32))
}

/// Runs a mixer change or meter read with interrupts on: a driver may wait on the device (control transfers wait on timer ticks), as
/// starting a stream does.
fn with_interrupts(change: impl FnOnce() -> i32) -> i32 {
    let was_on = crate::cpu::interrupts_save_and_disable();
    crate::cpu::interrupts_enable();
    let result = change();
    crate::cpu::interrupts_restore(was_on);
    result
}

pub fn mixer_set(output: u64, source: u64, gain: u64) -> i32 {
    let _usb = pin();
    let gain = gain.min(u64::from(u32::MAX)) as u32;
    with_interrupts(|| audio::mixer().map_or(fosfor_abi::tape::MIXER_NONE, |mixer| mixer.set(output as u32, source as u32, gain)))
}

pub fn mixer_serial() -> u64 {
    let _usb = pin();
    audio::mixer().map_or(0, |mixer| mixer.serial())
}

pub fn mixer_meters(out: u64, count: u64) -> u64 {
    let _usb = pin();
    let Some(mixer) = audio::mixer() else { return 0 };
    let mut meters = alloc::vec![fosfor_abi::tape::MixerMeter::default(); (count as usize).min(METERS_MAX)];
    // Meters may be bulk reads, which wait on the device like a change does.
    let mut got = 0;
    with_interrupts(|| {
        got = mixer.meters(&mut meters);
        0
    });
    let bytes = unsafe { core::slice::from_raw_parts(meters.as_ptr().cast::<u8>(), got * core::mem::size_of::<fosfor_abi::tape::MixerMeter>()) }; // kernel-memory
    if process::copy_to_user(out, bytes) { got as u64 } else { 0 }
}

pub fn mixer_control(index: u64, out: u64) -> i32 {
    let _usb = pin();
    let Some(mixer) = audio::mixer() else { return fosfor_abi::tape::MIXER_NONE };
    let mut control = fosfor_abi::tape::MixerControl::default();
    if mixer.control(index as u32, &mut control) && put(out, &control) { 0 } else { fosfor_abi::tape::MIXER_BAD }
}

pub fn mixer_control_item(index: u64, item: u64, buffer: u64, capacity: u64) -> i64 {
    let _usb = pin();
    let Some(mixer) = audio::mixer() else { return mixer_signed(fosfor_abi::tape::MIXER_NONE) };
    copy_name(buffer, capacity, |out| mixer.control_item(index as u32, item as u32, out))
}

pub fn mixer_control_set(index: u64, value: u64) -> i32 {
    let _usb = pin();
    with_interrupts(|| audio::mixer().map_or(fosfor_abi::tape::MIXER_NONE, |mixer| mixer.control_set(index as u32, value as i64)))
}
