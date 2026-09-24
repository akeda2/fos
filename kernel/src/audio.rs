//! Common kernel-owned audio-playback facade.
//!
//! Mirrors `nic.rs`'s own shape deliberately: a small enum wrapping every
//! supported backend, a lazy `initialize()`/`get()` pair, and a stable,
//! format-agnostic contract the tape ABI (`shell.rs`) and shell commands
//! (`audio`) build on, so a second codec driver slots in the same way a
//! second NIC backend already does, without either of those callers
//! changing.
//!
//! **Why this layer exists separately from `es1370.rs`:** ES1370 is a
//! *hard* 44.1 kHz/16-bit ceiling -- its DAC1 rate generator only offers
//! four hardwired rates (5512/11025/22050/44100 Hz) and its sample format
//! field only encodes 8- or 16-bit PCM (see `es1370.rs`'s own register
//! doc comments). That is a genuine silicon limit, not a driver choice,
//! and this project's real Sound Blaster PCI hardware target shares it.
//! A future card (e.g. an AC97 or HD-Audio codec, neither implemented
//! yet) could plausibly support 24-bit/48 kHz -- so nothing in this
//! facade, the ring buffer below, or the tape ABI's `audio_submit`
//! callback hardcodes ES1370's own format. A tape must always ask the
//! active device its real format (`AudioFormat`, via the ABI's
//! `audio_sample_rate`/`audio_format` callbacks) and produce exactly that
//! format -- this kernel does no sample-rate conversion or bit-depth
//! requantization of its own, the same "small kernel API, protocol/format
//! logic stays in tape code" split this project already applies to NIC
//! frame parsing.
//!
//! The ISR-driven refill design itself (see `es1370.rs`'s own module doc
//! comment for why a real interrupt handler is warranted here, unlike
//! every NIC backend's ack-only convention) and the lock-free ring buffer
//! below are both format-agnostic: they move raw bytes, at whatever
//! bytes-per-frame the active device's own `AudioFormat` implies, with no
//! assumption about bit depth or channel count baked in beyond that.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::es1370;
use crate::hda;

/// One playback device's fixed sample format. Never negotiated or
/// converted by this kernel -- a producer (an audio-streaming tape) must
/// query this before submitting samples and match it exactly, the same
/// way `nic::MAX_FRAME_LEN` is a fixed fact a tape must respect rather
/// than something the kernel adapts to the tape's own preference.
#[derive(Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

impl AudioFormat {
    /// Public API for a future audio-streaming tape to compute chunk
    /// sizing against the active device's real format -- not yet called
    /// from any kernel code path, since no such tape exists yet in this
    /// repository (see `audio-tape` in this project's own todo history).
    #[allow(dead_code)]
    pub fn bytes_per_frame(&self) -> u32 {
        self.channels as u32 * (self.bits_per_sample as u32 / 8)
    }
}

/// Capacity of the network-fed jitter ring buffer every backend owns one
/// of, in bytes. A first attempt tried sizing this generously against the
/// *worst* format this project expects to ever support (24-bit stereo at
/// 48 kHz, 384,000 bytes/sec) at 64 KiB -- but that measurably does not
/// fit: unlike every other large `.bss` array in this kernel (see
/// AGENTS.md's low-memory-map growth history), `RING_CAPACITY_BYTES` past
/// roughly 32 KiB crosses an LLVM codegen threshold where a `[0u8; N]`
/// initializer this large stops being provably foldable away even before
/// any caller actually reaches `submit`/`pop_into` -- confirmed by
/// bisection through the real `make kernel` build (not a bare `cargo
/// build`, which uses different `-Z build-std` flags and gave misleading
/// numbers): 32768 links, 40960 does not. 16 KiB was chosen as a real,
/// comfortably-inside-that-threshold size, covering ~93 ms at ES1370's
/// current 44.1 kHz/16-bit/stereo format (176,400 bytes/sec) -- plenty of
/// jitter tolerance for a LAN link -- and ~43 ms at the hypothetical
/// 24-bit/48 kHz worst case. A future codec driver needing more headroom
/// at that higher rate should re-measure the real link budget with
/// `audio_submit` actually wired live (this constant's history above is
/// the cautionary example of why: an unwired ring can silently appear to
/// cost nothing at all sizes up to that same threshold, only to reveal
/// its true cost once real code depends on it).
pub const RING_CAPACITY_BYTES: usize = 16384;

/// A fixed-capacity, lock-free, single-producer/single-consumer byte ring:
/// a tape's own foreground code (or a future protocol-parsing loop) is the
/// only producer, and one device's own ISR (running on the same single
/// core, but able to interrupt the producer at any point) is the only
/// consumer. This is exactly the same shape -- and the same safety
/// argument -- as `console`'s own `RELAY_QUEUE`/`interrupts`'s scancode
/// queue: monotonically increasing `write_pos`/`read_pos` byte counters
/// (never wrapped themselves, only their `% RING_CAPACITY_BYTES` index
/// into the backing array), with the producer publishing new bytes via a
/// `Release` store of `write_pos` only after every byte is written, and
/// the consumer reading `write_pos` with `Acquire` before touching any
/// byte it implies is valid -- sufficient on this project's x86 target
/// for the producer/consumer byte-visibility guarantee this needs, the
/// same reasoning already used for those two existing queues.
pub struct PcmRing {
    buffer: UnsafeCell<[u8; RING_CAPACITY_BYTES]>,
    write_pos: AtomicU32,
    read_pos: AtomicU32,
}

// Safety: every field is either an atomic or only ever mutated through the
// bounded, ordering-disciplined `push`/`pop_into` methods below, following
// the same single-producer/single-consumer contract `console::RELAY_QUEUE`
// already relies on in this single-core kernel.
unsafe impl Sync for PcmRing {}

impl PcmRing {
    pub const fn new() -> Self {
        PcmRing {
            buffer: UnsafeCell::new([0u8; RING_CAPACITY_BYTES]),
            write_pos: AtomicU32::new(0),
            read_pos: AtomicU32::new(0),
        }
    }

    /// Appends as many bytes of `data` as currently fit, returning how
    /// many were actually accepted (0 if the ring is completely full).
    /// Never blocks and never overwrites unread data -- a producer
    /// getting a short count back must retry the remainder later, the
    /// same "accepted length, caller paces itself" convention
    /// `nic_send`/`nic_receive`'s own ABI already uses.
    pub fn push(&self, data: &[u8]) -> usize {
        let write = self.write_pos.load(Ordering::Relaxed);
        let read = self.read_pos.load(Ordering::Acquire);
        let capacity = RING_CAPACITY_BYTES as u32;
        let used = write.wrapping_sub(read);
        let free = capacity.saturating_sub(used);
        let to_write = (data.len() as u32).min(free) as usize;
        for (i, &byte) in data[..to_write].iter().enumerate() {
            let index = (write as usize + i) % RING_CAPACITY_BYTES;
            unsafe {
                (*self.buffer.get())[index] = byte;
            }
        }
        self.write_pos
            .store(write.wrapping_add(to_write as u32), Ordering::Release);
        to_write
    }

    /// Copies exactly `len` bytes into `dest` (a raw pointer, so the ISR
    /// can write straight into a DMA buffer with no intermediate stack
    /// copy) and advances the read position -- but only if at least
    /// `len` bytes are actually available. This is deliberately
    /// all-or-nothing: a caller (the ISR's own period refill) needs to
    /// know unambiguously whether it got a full period's worth of real
    /// samples or none at all, so it can fall back to silence rather than
    /// play a partially-refilled, glitchy period.
    ///
    /// # Safety
    /// `dest` must be valid for `len` writes.
    pub unsafe fn pop_into(&self, dest: *mut u8, len: usize) -> bool {
        let read = self.read_pos.load(Ordering::Relaxed);
        let write = self.write_pos.load(Ordering::Acquire);
        let available = write.wrapping_sub(read) as usize;
        if available < len {
            return false;
        }
        for i in 0..len {
            let index = (read as usize + i) % RING_CAPACITY_BYTES;
            let byte = unsafe { (*self.buffer.get())[index] };
            unsafe {
                core::ptr::write_volatile(dest.add(i), byte);
            }
        }
        self.read_pos
            .store(read.wrapping_add(len as u32), Ordering::Release);
        true
    }

    pub fn available(&self) -> usize {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Relaxed);
        write.wrapping_sub(read) as usize
    }

    /// Copies up to `max_len` currently available bytes into `dest`,
    /// returning how many were actually copied (0 if none are
    /// available yet). Unlike `pop_into`, this is a variable-length,
    /// "however much is ready" pop rather than an all-or-nothing one --
    /// the right shape for a capture consumer (no fixed-period alignment
    /// to preserve, unlike DAC1's playback refill), mirroring
    /// `tape_nic_receive`'s own "copy whatever is waiting" convention.
    ///
    /// # Safety
    /// `dest` must be valid for `max_len` writes.
    pub unsafe fn pop_upto(&self, dest: *mut u8, max_len: usize) -> usize {
        let read = self.read_pos.load(Ordering::Relaxed);
        let write = self.write_pos.load(Ordering::Acquire);
        let available = write.wrapping_sub(read) as usize;
        let to_read = available.min(max_len);
        for i in 0..to_read {
            let index = (read as usize + i) % RING_CAPACITY_BYTES;
            let byte = unsafe { (*self.buffer.get())[index] };
            unsafe {
                core::ptr::write_volatile(dest.add(i), byte);
            }
        }
        self.read_pos
            .store(read.wrapping_add(to_read as u32), Ordering::Release);
        to_read
    }
}

pub enum Audio {
    Es1370(&'static es1370::Es1370),
    Hda(&'static hda::Hda),
}

impl Audio {
    pub fn driver_name(&self) -> &'static str {
        match self {
            Audio::Es1370(_) => "ES1370",
            Audio::Hda(_) => "Intel HDA",
        }
    }

    pub fn format(&self) -> AudioFormat {
        match self {
            Audio::Es1370(card) => card.format(),
            Audio::Hda(card) => card.format(),
        }
    }

    /// Submits decoded PCM samples for playback, in the active device's
    /// own `format()` -- never converted or resampled by this kernel.
    /// Returns how many bytes were actually accepted; a short count means
    /// the device's ring buffer is temporarily full and the caller should
    /// retry the remainder on its next poll.
    pub fn submit(&self, data: &[u8]) -> usize {
        match self {
            Audio::Es1370(card) => card.submit(data),
            Audio::Hda(card) => card.submit(data),
        }
    }

    pub fn underrun_count(&self) -> u32 {
        match self {
            Audio::Es1370(card) => card.underrun_count(),
            Audio::Hda(card) => card.underrun_count(),
        }
    }

    /// How many not-yet-played bytes currently sit in the network-fed
    /// ring, in bytes -- see `Es1370::ring_backlog`'s own doc comment.
    pub fn ring_backlog(&self) -> usize {
        match self {
            Audio::Es1370(card) => card.ring_backlog(),
            Audio::Hda(card) => card.ring_backlog(),
        }
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32) {
        match self {
            Audio::Es1370(card) => card.interrupt_stats(),
            Audio::Hda(card) => card.interrupt_stats(),
        }
    }

    /// A device's base address for diagnostics: a legacy I/O port number
    /// for ES1370, or an MMIO physical address for HDA -- widened to
    /// `u32` so both fit, printed with enough hex digits for either
    /// (`audio_status`'s own `0x{:08x}` format).
    pub fn io_base(&self) -> u32 {
        match self {
            Audio::Es1370(card) => card.io_base() as u32,
            Audio::Hda(card) => card.io_base(),
        }
    }

    pub fn raw_status(&self) -> u32 {
        match self {
            Audio::Es1370(card) => card.raw_status(),
            Audio::Hda(card) => card.raw_status(),
        }
    }

    /// Copies out captured (recorded) PCM samples, in the active
    /// device's own `format()`, up to `out.len()` bytes -- the ADC
    /// counterpart to `submit`. Returns how many bytes were actually
    /// copied; fewer than `out.len()` (including zero) simply means
    /// fewer samples have been captured so far, not an error. HDA
    /// playback-only in this driver's initial scope, so an HDA device
    /// always reports zero captured bytes here, exactly like ES1370
    /// before its own ADC support existed.
    pub fn capture(&self, out: &mut [u8]) -> usize {
        match self {
            Audio::Es1370(card) => card.capture(out),
            Audio::Hda(_) => 0,
        }
    }

    pub fn capture_overrun_count(&self) -> u32 {
        match self {
            Audio::Es1370(card) => card.capture_overrun_count(),
            Audio::Hda(_) => 0,
        }
    }

    /// How many not-yet-collected captured bytes currently sit in the
    /// capture ring -- see `Es1370::capture_backlog`'s own doc comment.
    pub fn capture_backlog(&self) -> usize {
        match self {
            Audio::Es1370(card) => card.capture_backlog(),
            Audio::Hda(_) => 0,
        }
    }

    /// How many ADC periods have been serviced since boot, the
    /// capture-side counterpart to `interrupt_stats`'s period count.
    pub fn capture_periods(&self) -> u32 {
        match self {
            Audio::Es1370(card) => card.capture_periods(),
            Audio::Hda(_) => 0,
        }
    }
}

/// Maximum simultaneously usable audio devices: `es1370::MAX_INSTANCES`
/// plus `hda::MAX_INSTANCES`, since both backends now share one flat
/// `DEVICES` array -- see `initialize`'s own doc comment for why there
/// is no per-backend cap here, only this one combined ceiling.
pub const MAX_DEVICES: usize = es1370::MAX_INSTANCES + hda::MAX_INSTANCES;

static mut DEVICES: [Option<Audio>; MAX_DEVICES] = [const { None }; MAX_DEVICES];
static mut DEVICE_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

/// Discovers and starts every supported audio backend instance found
/// (ES1370 and any Intel HDA controller reporting a usable playback
/// stream; a future codec driver's own instances would be appended the
/// same way a fifth NIC backend joins `nic::initialize`'s own loop).
/// Idempotent, mirroring `nic::initialize`.
///
/// Deliberately no "active device" selection concept here, unlike
/// `nic::set_active`: the whole point of supporting more than one
/// instance is to drive every discovered card *simultaneously* as
/// independent stereo pairs for a combined multichannel stream, not to
/// pick one at a time. `get()` keeps meaning "device 0" purely so
/// existing, non-indexed tape ABI callbacks (`tape_audio_sample_rate`,
/// `tape_audio_submit`, ...) keep working unchanged for a single-card
/// setup; a tape wanting every device must use `get_at`/`count` instead.
pub fn initialize() -> Result<usize, Status> {
    unsafe {
        if INITIALIZED {
            return if DEVICE_COUNT == 0 {
                Err(Status::NotPresent)
            } else {
                Ok(DEVICE_COUNT)
            };
        }
        INITIALIZED = true;
    }

    let found = match es1370::initialize_all() {
        Ok(count) => count,
        Err(es1370::Status::NotPresent) => 0,
        Err(es1370::Status::InitFailed) => 0,
    };

    unsafe {
        for index in 0..found.min(MAX_DEVICES) {
            if let Some(card) = es1370::get(index) {
                DEVICES[DEVICE_COUNT] = Some(Audio::Es1370(card));
                DEVICE_COUNT += 1;
            }
        }

        // HDA controllers with no usable playback stream (`has_playback`
        // false -- no responding codec, no usable DAC/pin pair, or no
        // free output stream at all) are deliberately left out of this
        // facade entirely: an `audio-net`-style tape driving every
        // `audio_count()` device has no use for one it could never
        // submit a single byte to, and `hda status`/the `hda` shell
        // command remain the right place to inspect such a controller's
        // node graph regardless of whether playback ever came up.
        let hda_found = hda::initialize_all().unwrap_or(0);
        for index in 0..hda_found {
            if DEVICE_COUNT >= MAX_DEVICES {
                break;
            }
            if let Some(controller) = hda::get(index) {
                if controller.has_playback() {
                    DEVICES[DEVICE_COUNT] = Some(Audio::Hda(controller));
                    DEVICE_COUNT += 1;
                }
            }
        }

        if DEVICE_COUNT == 0 {
            Err(Status::InitFailed)
        } else {
            Ok(DEVICE_COUNT)
        }
    }
}

/// The first discovered device, kept for every existing, non-indexed
/// caller (the tape ABI's plain `audio_sample_rate`/`audio_submit`/...
/// callbacks, and the pre-multi-instance `audio` shell command reporting)
/// -- see `initialize`'s own doc comment for why there is no separate
/// "active device" concept to select instead.
pub fn get() -> Result<&'static Audio, Status> {
    initialize()?;
    get_at(0).ok_or(Status::NotPresent)
}

/// Returns one discovered device by index (0-based, matching every other
/// `_at`-suffixed tape ABI convention in this project -- see
/// `nic_mac_at`'s own doc comment). Returns `None` for an index beyond
/// however many were actually found, never a partially initialized slot.
pub fn get_at(index: usize) -> Option<&'static Audio> {
    unsafe {
        if index >= DEVICE_COUNT {
            return None;
        }
        DEVICES[index].as_ref()
    }
}

/// How many audio devices were discovered, 0 before `initialize`/`get`
/// has ever been called successfully.
pub fn count() -> usize {
    unsafe { DEVICE_COUNT }
}
