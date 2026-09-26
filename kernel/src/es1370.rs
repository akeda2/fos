//! Minimal driver for the Ensoniq/Creative ES1370 ("AudioPCI") sound
//! controller.
//!
//! ES1370 was chosen as this project's first audio target the same way
//! RTL8139 was chosen as the first NIC: it is QEMU's own emulated sound
//! device (`-device ES1370`), so this driver is testable in CI right now,
//! without needing physical hardware for every iteration. Unlike RTL8139,
//! though, this chip family is *also* directly relevant to real hardware
//! this project expects to run on: Creative's early "Sound Blaster PCI"
//! cards (e.g. SB PCI64/128) are commonly a rebranded Ensoniq ES1370 or its
//! near-identical successor ES1371 (which adds an AC97 codec in place of
//! ES1370's own DAC/mixer, but keeps the same DMA/interrupt register
//! shape). A Sound Blaster Live!/Audigy card is a *different*, unrelated
//! chip (Creative's own EMU10K1/EMU10K2 DSP), not part of this family --
//! confirm which family a given physical card actually is via the `pci`
//! shell command before assuming this driver applies to it, the same
//! caution this project already applies to every other NIC/serial card.
//!
//! Every register offset, bit, and DMA semantic below was cross-checked
//! against QEMU's own `hw/audio/es1370.c` emulation (itself transcribed,
//! with attribution, from Linux's historic `drivers/sound/es1370.c`), not
//! reconstructed from memory -- the same rigor this project applies to
//! every other register-level driver after the RTL8139 TCR/interframe-gap
//! lessons (see AGENTS.md's "Real hardware notes"). It is, as of this
//! writing, **unverified against real hardware** -- expect real-hardware
//! fixups the same way every other PCI driver here needed them.
//!
//! DAC1 (one stereo 16-bit playback channel) and the ADC (one stereo
//! 16-bit recording channel) are implemented; DAC2, the MIDI UART, and
//! the joystick port are left untouched. Real hardware also has an
//! external mixer chip (commonly an AK4531) reachable through the
//! `CODEC` indirect register; this driver does not program it, so a
//! physical card may need its master/DAC volume unmuted, or its
//! recording source selected (line-in vs. mic, `CONTROL`'s `XCTL0` bit;
//! electret mic bias, `XCTL1`), through a separate path before audio is
//! actually audible or a real microphone signal reaches the ADC at all --
//! QEMU's emulated backend has no such gating on either direction, so
//! this gap does not block testing here.
//!
//! **This is the first driver in this kernel whose interrupt handler does
//! real, non-trivial work**, a deliberate divergence from every other PCI
//! backend's "ack/count/flag only, leave all real work to a foreground
//! polling loop" convention (see `nic.rs`'s own doc comment). That
//! convention exists because NIC frame parsing is unbounded and
//! protocol-shaped -- exactly the kind of thing that does not belong in an
//! interrupt handler. Refilling one half of a fixed-size, double-buffered
//! DMA ring with exactly `PERIOD_FRAMES` samples is the opposite: small,
//! bounded, and -- unlike NIC receive -- *time-critical*, since the
//! playback hardware will loop back and replay stale data the instant a
//! foreground loop is slow to notice. Doing the refill directly in the ISR
//! is what actually earns the "real-time" framing discussed for this
//! project's audio-streaming goal.

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::x86::{inl, outb, outl};
use crate::audio;
use crate::interrupts;
use crate::pci;
use crate::pmem;

const VENDOR_ENSONIQ: u16 = 0x1274;
const DEVICE_ES1370: u16 = 0x5000;

// Register offsets from the card's I/O base (BAR0), transcribed verbatim
// from QEMU's `ES1370_REG_*` constants.
const REG_CONTROL: u16 = 0x00;
const REG_STATUS: u16 = 0x04;
const REG_MEMPAGE: u16 = 0x0c;
const REG_SERIAL_CONTROL: u16 = 0x20;
const REG_DAC1_SCOUNT: u16 = 0x24;
const REG_ADC_SCOUNT: u16 = 0x2c;

/// The page-selected indirect window (`0x30..=0x3f`) holds a different
/// channel's frame-address/frame-count pair depending on which page
/// `REG_MEMPAGE` currently selects. DAC1 and DAC2 share page `0x0c`
/// (DAC1 at `0x30`/`0x34`, DAC2 at `0x38`/`0x3c`); the ADC pair lives on
/// page `0x0d` at the same `0x30`/`0x34` offsets -- so the ADC path below
/// reuses `REG_DAC1_FRAMEADR`/`REG_DAC1_FRAMECNT` unchanged, just with
/// `PAGE_ADC` selected first, exactly as QEMU's own `es1370.c` reuses one
/// pair of case labels across both windows via its `mempage` fixup.
const PAGE_DAC: u8 = 0x0c;
const PAGE_ADC: u8 = 0x0d;
const REG_DAC1_FRAMEADR: u16 = 0x30;
const REG_DAC1_FRAMECNT: u16 = 0x34;

/// `CONTROL`'s `WTSRSEL` field (bits 13:12) selects DAC1's fixed sample
/// rate from a hardwired table (5512/11025/22050/44100 Hz) -- unlike DAC2,
/// DAC1 cannot run at an arbitrary rate. `0b11` selects the last entry,
/// 44100 Hz.
const CTRL_WTSRSEL_MASK: u32 = 0b11 << 12;
const CTRL_WTSRSEL_44100: u32 = 0b11 << 12;
const CTRL_ADC_EN: u32 = 1 << 4;
const CTRL_DAC1_EN: u32 = 1 << 6;
/// Matches this chip's power-on-reset default (`ctl = 1`); kept set so
/// this driver's own reset write doesn't have to also decide whether to
/// change the PCI SERR-signal policy firmware already configured.
const CTRL_SERR_DIS: u32 = 1 << 0;
/// `CONTROL`'s `PCLKDIV` field (bits 28:16) is the shared clock divider
/// for the ADC *and* DAC2 -- unlike DAC1, both run off an arbitrary
/// programmable-clock-generator rate rather than a small hardwired table,
/// per Linux's historic `es1370.c` (`DAC2_SRTODIV`/`DAC2_DIVTOSR`:
/// `rate = 1411200 / (divider + 2)`). This driver only ever runs the ADC
/// at 44100 Hz (matching `format()`'s advertised fixed rate below), so the
/// divider is a precomputed constant, not a runtime formula: solving for
/// 44100 gives `((1411200 + 44100/2) / 44100) - 2 = 30`.
const CTRL_PCLKDIV_MASK: u32 = 0x1fff << 16;
const CTRL_PCLKDIV_44100: u32 = 30 << 16;

/// `SERIAL_CONTROL`'s DAC1 interrupt-enable bit.
const SCTRL_P1INTEN: u32 = 1 << 8;
/// `SERIAL_CONTROL`'s DAC1 format field (bits 1:0): bit0 = stereo, bit1 =
/// 16-bit. `0b11` selects 16-bit stereo, the only format this driver
/// programs.
const SCTRL_P1FMT_STEREO16: u32 = 0b11;
/// `SERIAL_CONTROL`'s ADC ("R1", for "record 1") interrupt-enable bit.
const SCTRL_R1INTEN: u32 = 1 << 10;
/// `SERIAL_CONTROL`'s ADC format field (bits 5:4), the same two-bit
/// stereo/16-bit shape as `SCTRL_P1FMT_STEREO16` just shifted up four
/// bits for this channel.
const SCTRL_R1FMT_STEREO16: u32 = 0b11 << 4;

/// `STATUS`'s "DAC1 half/period complete" bit.
const STAT_DAC1: u32 = 1 << 2;
/// `STATUS`'s "ADC half/period complete" bit.
const STAT_ADC: u32 = 1;

/// Bytes per stereo 16-bit sample-frame (2 channels x 2 bytes). This
/// happens to equal `FRAMECNT`'s own 4-byte "dword" counting unit, which
/// is what lets this driver treat `SCOUNT`'s period length and
/// `FRAMECNT`'s buffer-size field in the same units without a separate
/// conversion -- true only for this exact format, not in general.
const BYTES_PER_FRAME: u32 = 4;

/// One playback period, in sample-frames -- roughly 23 ms at 44.1 kHz.
/// Generous for a first working version, matching this project's own
/// documented preference (see the RTL8139 ring-size lesson in
/// AGENTS.md) for picking the smallest buffer that survives measured
/// real-world jitter, not the largest one available. Shrinking this is
/// the natural next tuning step once ISR-driven refill is confirmed
/// glitch-free end to end.
const PERIOD_FRAMES: u32 = 1024;
/// Double-buffered: while hardware plays one period, the ISR refills the
/// other.
const PERIOD_COUNT: u32 = 2;
const TOTAL_FRAMES: u32 = PERIOD_FRAMES * PERIOD_COUNT;
const BUFFER_BYTES: u32 = TOTAL_FRAMES * BYTES_PER_FRAME;

fn write_reg32(io_base: u16, reg: u16, value: u32) {
    unsafe {
        outl(io_base + reg, value);
    }
}

fn read_reg32(io_base: u16, reg: u16) -> u32 {
    unsafe { inl(io_base + reg) }
}

fn select_page(io_base: u16, page: u8) {
    unsafe {
        outb(io_base + REG_MEMPAGE, page);
    }
}

#[derive(Debug, Clone, Copy)]
pub enum Status {
    NotPresent,
    InitFailed,
}

/// How many independent ES1370 (or compatible) cards this driver will
/// discover and drive simultaneously. Each card is a completely
/// independent stereo pair -- this chip's DAC1 has no way to address more
/// than 2 channels itself (see this module's own doc comment) -- so this
/// is also this project's current ceiling on ES1370-backed channel count:
/// `MAX_INSTANCES * 2` (4, at the current value of 2). Deliberately not
/// `pci::MAX_DEVICE_MATCHES` (8): each instance owns two independent
/// `audio::RING_CAPACITY_BYTES`-sized (16 KiB) jitter rings (playback and
/// capture), so this constant directly trades off against the kernel's
/// own tight reserved-low-memory-region budget (see AGENTS.md's
/// "Low-memory map" growth history). Originally 4 (enough to reach an
/// earlier "4 or 8 channels" target); reduced to 2 -- still comfortably
/// above what a real, physically installed set of vintage Sound Blaster
/// PCI cards is ever likely to need simultaneously -- to reclaim ~65 KiB
/// of `.bss` for AC'97 (`ac97.rs`) once the reserved region's own real
/// remaining headroom turned out to be only double-digit bytes, not the
/// low tens of KiB a stale measurement had assumed.
pub const MAX_INSTANCES: usize = 2;

pub struct Es1370 {
    /// Which slot in `RINGS`/`CAPTURE_RINGS` (and, from the facade's own
    /// point of view, `audio::DEVICES`) this instance owns. Needed
    /// because those rings are top-level statics, not fields of this
    /// struct -- see `RINGS`'s own doc comment for why -- so every method
    /// that used to reference a single top-level `RING`/`CAPTURE_RING`
    /// now indexes through this instead.
    instance: usize,
    io_base: u16,
    irq: Option<u8>,
    buffer_addr: u32,
    /// Running sample counter for the built-in test-tone generator (see
    /// `fill_period_tone`), kept continuous across periods so the
    /// waveform has no discontinuity at a refill boundary.
    tone_phase: Cell<u32>,
    interrupt_count: AtomicU32,
    /// Counts every serviced period since playback started; also used,
    /// mod `PERIOD_COUNT`, to pick which half of the ring to refill next
    /// (see `handle_interrupt`'s doc comment for why this needs no
    /// hardware position read at all).
    periods_serviced: AtomicU32,
    /// Set permanently, the first time anything is ever submitted via
    /// `submit`. Before that point, `fill_period` plays the built-in test
    /// tone (useful for `audio`-command smoke testing with no network
    /// transport at all); afterward, an empty ring means silence, never a
    /// fallback back to the tone -- mixing a stale test waveform into a
    /// real stream would be far more confusing than an audible gap.
    network_mode: core::sync::atomic::AtomicBool,
    /// Counts periods where `fill_period` found fewer than one full
    /// period's worth of real samples queued, once `network_mode` was
    /// already active -- the audio-side equivalent of an RTL8139
    /// overflow counter, surfaced by the `audio` shell command.
    underrun_count: AtomicU32,
    /// Physical base of the ADC's own DMA ring, entirely separate from
    /// `buffer_addr` (DAC1's playback ring) -- capture and playback run
    /// independently, each with its own hardware frame-address/count
    /// pair and its own half-tracking counter below, so one running
    /// without the other (or both at different, coincidentally
    /// overlapping moments) needs no special-casing.
    capture_buffer_addr: u32,
    /// Counts every serviced ADC period since capture started; the
    /// `handle_interrupt` counterpart to `periods_serviced`.
    periods_captured: AtomicU32,
    /// Counts periods where a captured period could not be fully queued
    /// into `CAPTURE_RING` because a consumer (a tape's own
    /// `audio_capture` polling) was not draining it fast enough -- the
    /// capture-side equivalent of `underrun_count`, but named
    /// "overrun" since here it is the *producer* (hardware, via the ISR)
    /// that has more data than the ring can currently hold, the opposite
    /// direction of DAC1's "consumer ran out of data" case.
    capture_overrun_count: AtomicU32,
}

static mut CARDS: [Option<Es1370>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CARD_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

/// The kernel-owned jitter buffers each instance's foreground code feeds
/// via `Es1370::submit`/the ABI's `audio_submit`/`audio_submit_at`
/// callbacks (see `audio.rs`'s own doc comment for the full
/// producer/consumer contract). One slot per possible card, indexed by
/// `Es1370::instance` -- deliberately a fixed-size array of standalone
/// top-level statics, not a field embedded in `Es1370` itself: at
/// `audio::RING_CAPACITY_BYTES` (16 KiB) each, embedding one inside a
/// value ever constructed as a local (as `initialize_all`'s own `let
/// card = Es1370 { ... }` briefly does, before moving it into `CARDS`)
/// would risk a large, avoidable stack copy on this kernel's own bounded
/// stack; a `static` array with a `const` per-element initializer is
/// instead placed directly in `.bss` at link time, with no runtime copy
/// at all. This mirrors exactly why `TAPE_INPUT_QUEUE`/
/// `console::RELAY_QUEUE` are standalone statics rather than fields
/// embedded in some larger struct.
static RINGS: [audio::PcmRing; MAX_INSTANCES] =
    [const { audio::PcmRing::new() }; MAX_INSTANCES];

/// The ADC's own jitter rings, symmetric to `RINGS` but flowing the
/// opposite direction: each instance's own ISR (`drain_capture_period`)
/// is the sole producer, and a tape's own foreground polling (via the
/// ABI's `audio_capture`/`audio_capture_at` callbacks, `Es1370::capture`)
/// is the sole consumer. Kept as a second, independent array rather than
/// reusing `RINGS` in the other direction, so a card that both plays back
/// and captures (a future full-duplex application) never has one
/// direction's backlog interfere with the other's.
static CAPTURE_RINGS: [audio::PcmRing; MAX_INSTANCES] =
    [const { audio::PcmRing::new() }; MAX_INSTANCES];

impl Es1370 {
    fn reset(&self) {
        // Matches this chip's own power-on-reset values: every channel
        // disabled, no interrupts enabled.
        write_reg32(self.io_base, REG_CONTROL, CTRL_SERR_DIS);
        write_reg32(self.io_base, REG_SERIAL_CONTROL, 0);
    }

    /// This device's fixed native format -- see `audio.rs`'s own doc
    /// comment for why this is a genuine hardware ceiling (DAC1's rate
    /// generator and sample-format field are both hardwired, small
    /// lookup tables), not a driver-chosen default that could simply be
    /// widened later. A tape must query this and produce exactly this
    /// format; this driver performs no conversion of its own. This is
    /// also the ADC's own recording format: unlike DAC1, the ADC's rate
    /// generator (`CONTROL`'s `PCLKDIV` field) genuinely could run at an
    /// arbitrary rate, but `start_adc` only ever programs it for 44100 Hz
    /// so a single `format()` describes both directions -- a future
    /// full-duplex tape never needs to ask twice or handle a mismatch.
    pub fn format(&self) -> audio::AudioFormat {
        audio::AudioFormat {
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
        }
    }

    /// Accepts decoded PCM bytes from a tape (via the ABI's
    /// `audio_submit` callback), in this device's own `format()`, and
    /// switches this card permanently into network-fed mode on first use.
    /// Returns how many bytes were actually queued; a caller getting back
    /// fewer bytes than it offered should retry the remainder shortly,
    /// exactly like `nic_send`'s own short-write convention.
    pub fn submit(&self, data: &[u8]) -> usize {
        self.network_mode
            .store(true, core::sync::atomic::Ordering::Relaxed);
        RINGS[self.instance].push(data)
    }

    pub fn underrun_count(&self) -> u32 {
        self.underrun_count.load(Ordering::Relaxed)
    }

    /// How many not-yet-played bytes currently sit in the network-fed
    /// ring, for diagnostic reporting (`audio` shell command) -- useful
    /// for telling a healthy, steadily-fed stream apart from one about to
    /// underrun, without waiting for an actual underrun to happen first.
    pub fn ring_backlog(&self) -> usize {
        RINGS[self.instance].available()
    }

    /// Copies up to `out.len()` freshly captured PCM bytes (in this
    /// device's own `format()`) out of `CAPTURE_RING`, returning how many
    /// bytes were actually copied -- zero if none have arrived yet. Unlike
    /// `submit`'s ring, which is all-or-nothing on the *consumer*
    /// (playback) side because a partially refilled period would glitch,
    /// this is a variable-length pop mirroring `tape_nic_receive`'s own
    /// "however much is ready right now" convention: a capture consumer
    /// has no equivalent fixed-period alignment requirement to preserve.
    pub fn capture(&self, out: &mut [u8]) -> usize {
        unsafe { CAPTURE_RINGS[self.instance].pop_upto(out.as_mut_ptr(), out.len()) }
    }

    pub fn capture_overrun_count(&self) -> u32 {
        self.capture_overrun_count.load(Ordering::Relaxed)
    }

    /// How many not-yet-collected captured bytes currently sit in
    /// `CAPTURE_RING`, the capture-side counterpart to `ring_backlog`.
    pub fn capture_backlog(&self) -> usize {
        CAPTURE_RINGS[self.instance].available()
    }

    /// Fills one period's worth of the DMA ring. Once `network_mode` is
    /// active, this pulls a full period from `ring`, falling back to
    /// silence (never the test tone -- see `network_mode`'s own doc
    /// comment) on an underrun; before that, it plays the built-in test
    /// tone so the `audio` shell command remains useful with no network
    /// transport wired up at all.
    fn fill_period(&self, half: u32) {
        let offset = half * PERIOD_FRAMES * BYTES_PER_FRAME;
        let base = (self.buffer_addr + offset) as *mut u8;
        let period_bytes = (PERIOD_FRAMES * BYTES_PER_FRAME) as usize;

        if self
            .network_mode
            .load(core::sync::atomic::Ordering::Relaxed)
        {
            let filled = unsafe { RINGS[self.instance].pop_into(base, period_bytes) };
            if !filled {
                self.underrun_count.fetch_add(1, Ordering::Relaxed);
                for i in 0..period_bytes {
                    unsafe {
                        core::ptr::write_volatile(base.add(i), 0u8);
                    }
                }
            }
            return;
        }

        self.fill_period_tone(base);
    }

    /// A fixed, continuous test tone (a ~441 Hz square wave, identical on
    /// both channels), used only before this card's first `submit` call
    /// (see `network_mode`).
    fn fill_period_tone(&self, base: *mut u8) {
        const CYCLE_FRAMES: u32 = 50; // 44100 / 50 / 2 ~= 441 Hz.
        const AMPLITUDE: i16 = 8000; // Comfortably below full scale.

        let mut n = self.tone_phase.get();
        for frame in 0..PERIOD_FRAMES {
            let sample: i16 = if (n / CYCLE_FRAMES) % 2 == 0 {
                AMPLITUDE
            } else {
                -AMPLITUDE
            };
            let bytes = sample.to_le_bytes();
            unsafe {
                let sample_addr = base.add((frame * BYTES_PER_FRAME) as usize);
                // Left and right channels carry the identical sample --
                // this is a mono test tone played on both.
                core::ptr::write_volatile(sample_addr, bytes[0]);
                core::ptr::write_volatile(sample_addr.add(1), bytes[1]);
                core::ptr::write_volatile(sample_addr.add(2), bytes[0]);
                core::ptr::write_volatile(sample_addr.add(3), bytes[1]);
            }
            n = n.wrapping_add(1);
        }
        self.tone_phase.set(n);
    }

    /// The ADC-side counterpart to `fill_period`: copies one just-completed
    /// period's worth of captured PCM bytes out of the ADC's own DMA ring
    /// and pushes them into `CAPTURE_RING` for a tape to later drain via
    /// `capture`. A short push (the ring already has a backlog a
    /// consumer hasn't collected yet) is counted in
    /// `capture_overrun_count` and the remaining, unqueued bytes are
    /// simply dropped -- exactly like `fill_period`'s underrun case, this
    /// protocol has no retransmission of "the samples that didn't fit".
    fn drain_capture_period(&self, half: u32) {
        let offset = half * PERIOD_FRAMES * BYTES_PER_FRAME;
        let base = (self.capture_buffer_addr + offset) as *const u8;
        let period_bytes = (PERIOD_FRAMES * BYTES_PER_FRAME) as usize;
        let captured = unsafe { core::slice::from_raw_parts(base, period_bytes) };
        let accepted = CAPTURE_RINGS[self.instance].push(captured);
        if accepted < period_bytes {
            self.capture_overrun_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn start_dac1(&self) {
        select_page(self.io_base, PAGE_DAC);
        write_reg32(self.io_base, REG_DAC1_FRAMEADR, self.buffer_addr);
        // FRAMECNT packs the buffer size (in dwords, minus one) in its low
        // 16 bits and the current playback position in its high 16 bits;
        // starting position 0 is correct since playback has not begun yet.
        let size_dwords_minus_one = (TOTAL_FRAMES * BYTES_PER_FRAME / 4) - 1;
        write_reg32(self.io_base, REG_DAC1_FRAMECNT, size_dwords_minus_one);
        // SCOUNT's period length is counted in the same 4-byte units as
        // FRAMECNT for 16-bit stereo specifically (see BYTES_PER_FRAME's
        // doc comment), so no separate conversion is needed here either.
        write_reg32(self.io_base, REG_DAC1_SCOUNT, PERIOD_FRAMES - 1);
        // Both CONTROL and SERIAL_CONTROL are shared across channels (DAC1
        // and the ADC alike), so this reads the register's current value
        // back and only ORs/clears this channel's own bits, rather than
        // overwriting the whole register -- preserving whatever the other
        // channel's own `start_*` call already programmed, regardless of
        // which one happens to run first.
        let sctrl = read_reg32(self.io_base, REG_SERIAL_CONTROL);
        write_reg32(
            self.io_base,
            REG_SERIAL_CONTROL,
            sctrl | SCTRL_P1INTEN | SCTRL_P1FMT_STEREO16,
        );
        let ctrl = read_reg32(self.io_base, REG_CONTROL);
        write_reg32(
            self.io_base,
            REG_CONTROL,
            (ctrl & !CTRL_WTSRSEL_MASK) | CTRL_WTSRSEL_44100 | CTRL_DAC1_EN,
        );
    }

    /// The ADC counterpart to `start_dac1`. Programs the ADC's own
    /// frame-address/frame-count/period-length registers (the same
    /// `0x30`/`0x34`/`0x2c` offsets DAC1 uses, but with `PAGE_ADC`
    /// selected instead of `PAGE_DAC` for the paged pair -- see
    /// `PAGE_ADC`'s own doc comment), then enables the ADC's interrupt
    /// and 16-bit-stereo format bits in `SERIAL_CONTROL` and its enable
    /// bit plus fixed-44100 `PCLKDIV` divider in `CONTROL` -- using the
    /// same read-modify-write discipline `start_dac1` does, so starting
    /// either channel never clobbers the other's already-programmed
    /// bits regardless of call order.
    fn start_adc(&self) {
        select_page(self.io_base, PAGE_ADC);
        write_reg32(self.io_base, REG_DAC1_FRAMEADR, self.capture_buffer_addr);
        let size_dwords_minus_one = (TOTAL_FRAMES * BYTES_PER_FRAME / 4) - 1;
        write_reg32(self.io_base, REG_DAC1_FRAMECNT, size_dwords_minus_one);
        write_reg32(self.io_base, REG_ADC_SCOUNT, PERIOD_FRAMES - 1);
        let sctrl = read_reg32(self.io_base, REG_SERIAL_CONTROL);
        write_reg32(
            self.io_base,
            REG_SERIAL_CONTROL,
            sctrl | SCTRL_R1INTEN | SCTRL_R1FMT_STEREO16,
        );
        let ctrl = read_reg32(self.io_base, REG_CONTROL);
        write_reg32(
            self.io_base,
            REG_CONTROL,
            (ctrl & !CTRL_PCLKDIV_MASK) | CTRL_PCLKDIV_44100 | CTRL_ADC_EN,
        );
    }

    /// Called from this driver's PCI IRQ handler once per elapsed
    /// playback period. Unlike every other backend's `handle_interrupt`,
    /// this one does real work (see this module's own doc comment for
    /// why that is deliberate here).
    ///
    /// Hardware loops through the two halves of the ring continuously
    /// (`0, 1, 0, 1, ...`) once started, entirely on its own, and raises
    /// `STAT_DAC1` each time it finishes one period. Because playback
    /// always starts at half 0, the Nth interrupt (0-indexed) always means
    /// "half `N mod PERIOD_COUNT` just finished playing and hardware has
    /// moved on to the other half" -- so a plain software counter is
    /// enough to know which half to refill next; no `FRAMECNT` read-back
    /// is needed at all.
    pub fn handle_interrupt(&self) {
        let status = read_reg32(self.io_base, REG_STATUS);
        if status & STAT_DAC1 != 0 {
            self.interrupt_count.fetch_add(1, Ordering::Relaxed);
            let served = self.periods_serviced.fetch_add(1, Ordering::Relaxed);
            self.fill_period(served % PERIOD_COUNT);
            self.acknowledge_dac1();
        }
        if status & STAT_ADC != 0 {
            let captured = self.periods_captured.fetch_add(1, Ordering::Relaxed);
            self.drain_capture_period(captured % PERIOD_COUNT);
            self.acknowledge_adc();
        }
    }

    /// Lowers `STATUS`'s `STAT_DAC1` bit (and, with it, this device's PCI
    /// `INTx#` level) so a legacy, level-triggered PCI interrupt line does
    /// not stay asserted forever.
    ///
    /// `STATUS` itself is read-only on real ES1370 hardware -- there is no
    /// "write here to acknowledge" register at all. The only documented
    /// way to clear a set `STAT_DAC1` is the transition `SERIAL_CONTROL`'s
    /// `P1INTEN` bit is required to make: from 1 to 0. Leaving this out
    /// entirely was this driver's original bug, and it is worth spelling
    /// out exactly how badly it failed, since the failure mode was easy to
    /// misdiagnose as "the interrupt never fires" rather than what it
    /// actually was: `STAT_DAC1` is a level, not an edge -- once the first
    /// period legitimately completes and raises it, an unacknowledged
    /// level stays asserted forever. The instant this handler's `iret`
    /// restores `EFLAGS.IF`, the still-asserted line refires *immediately*,
    /// before any other instruction (including one that could print a
    /// counter or read a keystroke) ever runs again -- confirmed directly
    /// against a QEMU `-d int` trace, which showed the exact same faulting
    /// `EIP` re-entered over 100,000 times within a couple of seconds --
    /// a genuine interrupt storm, not silence. A raw IRQ fire counter (`interrupts::pci_irq_count`) sampled from the shell
    /// afterward reads back as if nothing had ever happened, because the
    /// one command that could have shown a nonzero count never got the
    /// CPU time to run again once the storm began. Toggling `P1INTEN` off
    /// then back on (rather than leaving it permanently disabled) both
    /// clears the level for this period and keeps the next period's
    /// interrupt enabled, exactly mirroring the real driver convention
    /// this chip's own datasheet documents.
    ///
    /// This reads `SERIAL_CONTROL` back first and only ever clears/sets
    /// `P1INTEN` within whatever value it finds, rather than writing a
    /// fixed pattern -- once the ADC's own `R1INTEN`/format bits can also
    /// be live in this same register, a fixed pattern would silently
    /// clobber them every time DAC1 acknowledges a period.
    fn acknowledge_dac1(&self) {
        let current = read_reg32(self.io_base, REG_SERIAL_CONTROL);
        write_reg32(self.io_base, REG_SERIAL_CONTROL, current & !SCTRL_P1INTEN);
        write_reg32(self.io_base, REG_SERIAL_CONTROL, current | SCTRL_P1INTEN);
    }

    /// The ADC counterpart to `acknowledge_dac1`, toggling `R1INTEN`
    /// instead of `P1INTEN` to lower `STATUS`'s `STAT_ADC` bit -- see that
    /// function's own doc comment for why this must be a read-modify-write
    /// against whatever `SERIAL_CONTROL` currently holds, not a fixed
    /// pattern.
    fn acknowledge_adc(&self) {
        let current = read_reg32(self.io_base, REG_SERIAL_CONTROL);
        write_reg32(self.io_base, REG_SERIAL_CONTROL, current & !SCTRL_R1INTEN);
        write_reg32(self.io_base, REG_SERIAL_CONTROL, current | SCTRL_R1INTEN);
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32) {
        (
            self.irq,
            self.interrupt_count.load(Ordering::Relaxed),
            self.periods_serviced.load(Ordering::Relaxed),
        )
    }

    /// The capture-side counterpart to `interrupt_stats`: periods
    /// serviced by the ADC path since boot.
    pub fn capture_periods(&self) -> u32 {
        self.periods_captured.load(Ordering::Relaxed)
    }

    pub fn io_base(&self) -> u16 {
        self.io_base
    }

    /// Raw read of the `STATUS` register, exposed for diagnostics (`audio`
    /// shell command): lets us tell "the card reports a pending interrupt
    /// but this kernel never received it" apart from "the card genuinely
    /// never asserts one", while debugging IRQ delivery.
    pub fn raw_status(&self) -> u32 {
        read_reg32(self.io_base, REG_STATUS)
    }
}

/// Shared legacy IRQs are normal (see `nic.rs`'s own doc comment on this
/// same point, and AGENTS.md's "A real bug this test found: shared
/// legacy PCI IRQ lines silently broke a second device's `init()`" for
/// why this matters in practice -- two ES1370 cards, or an ES1370 next to
/// a NIC, can easily land on the same INTx line). Each card only reads
/// and acknowledges its own `STATUS` register, so calling every
/// registered card's `handle_interrupt` on every fire is always safe,
/// even for a card that was not the actual source this time.
fn handle_pci_irq(irq: u8) {
    unsafe {
        for index in 0..CARD_COUNT {
            if let Some(card) = CARDS[index].as_ref() {
                if card.irq == Some(irq) {
                    card.handle_interrupt();
                }
            }
        }
    }
}

/// Discovers and starts every usable ES1370 (up to `MAX_INSTANCES`),
/// retaining each instance's own DMA buffers for the remainder of the
/// boot. Idempotent, mirroring `rtl8139::initialize_all`: a second call
/// after a successful first one returns the already-discovered count
/// immediately without touching hardware again.
pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if INITIALIZED {
            return if CARD_COUNT == 0 {
                Err(Status::NotPresent)
            } else {
                Ok(CARD_COUNT)
            };
        }
        INITIALIZED = true;
    }

    let mut matches: [Option<pci::PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_devices(VENDOR_ENSONIQ, &[DEVICE_ES1370], &mut matches);
    if found == 0 {
        return Err(Status::NotPresent);
    }

    let mut init_failed = false;
    for address in matches[..found.min(MAX_INSTANCES)].iter().flatten() {
        if init_card(*address).is_err() {
            init_failed = true;
        }
    }

    unsafe {
        if CARD_COUNT == 0 {
            if init_failed {
                Err(Status::InitFailed)
            } else {
                Err(Status::NotPresent)
            }
        } else {
            Ok(CARD_COUNT)
        }
    }
}

/// Brings up one already-PCI-discovered card and, on success, appends it
/// to `CARDS`. Split out of `initialize_all` purely so that function's
/// own per-address loop reads as a plain list of attempts, one per
/// discovered card, mirroring `rtl8139::initialize_all`'s own shape.
fn init_card(address: pci::PciAddress) -> Result<(), Status> {
    let instance = unsafe { CARD_COUNT };
    if instance >= MAX_INSTANCES {
        return Err(Status::InitFailed);
    }

    address.enable_io_and_bus_master();
    let Some(io_base) = address.io_bar0() else {
        return Err(Status::InitFailed);
    };
    let irq = address.interrupt_line();

    let buffer_frames = (BUFFER_BYTES as usize).div_ceil(pmem::FRAME_SIZE as usize);
    let Some(buffer_addr) = pmem::allocate_contiguous(buffer_frames) else {
        return Err(Status::InitFailed);
    };
    // A separate physically contiguous DMA region for the ADC's own
    // ring, entirely independent of `buffer_addr` -- see
    // `capture_buffer_addr`'s own doc comment.
    let Some(capture_buffer_addr) = pmem::allocate_contiguous(buffer_frames) else {
        return Err(Status::InitFailed);
    };

    let card = Es1370 {
        instance,
        io_base,
        irq,
        buffer_addr,
        tone_phase: Cell::new(0),
        interrupt_count: AtomicU32::new(0),
        periods_serviced: AtomicU32::new(0),
        network_mode: core::sync::atomic::AtomicBool::new(false),
        underrun_count: AtomicU32::new(0),
        capture_buffer_addr,
        periods_captured: AtomicU32::new(0),
        capture_overrun_count: AtomicU32::new(0),
    };

    card.reset();
    card.fill_period(0);
    card.fill_period(1);

    if let Some(irq) = irq {
        if !interrupts::register_pci_irq(irq, handle_pci_irq) {
            return Err(Status::InitFailed);
        }
    }
    card.start_dac1();
    // The ADC is started unconditionally, alongside DAC1, the same way
    // DAC1 always plays something (network audio or the built-in test
    // tone) from the moment this driver initializes -- not gated behind
    // a tape's first `capture` call. This keeps both channels' state
    // machines equally simple (no separate "has recording ever been
    // requested" flag to thread through `handle_interrupt`) at a
    // negligible real cost: an unread capture ring merely drops
    // captured periods once full (counted in `capture_overrun_count`),
    // exactly as harmless as a real sound card capturing from a room
    // nobody is listening to.
    card.start_adc();

    unsafe {
        CARDS[instance] = Some(card);
        CARD_COUNT += 1;
    }
    Ok(())
}

/// Returns one already-discovered instance by index (0-based, unlike the
/// shell's own 1-based `nic use N` convention -- see `nic_mac_at`'s own
/// doc comment for why the tape ABI already made this same choice).
/// Assumes `initialize_all` has already run; callers reach this only
/// through `audio.rs`'s own facade, which calls it exactly once during
/// its own `initialize`.
pub fn get(index: usize) -> Option<&'static Es1370> {
    unsafe {
        if index >= CARD_COUNT {
            return None;
        }
        CARDS[index].as_ref()
    }
}
