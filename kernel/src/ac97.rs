//! Minimal driver for the Intel ICH-family AC'97 ("Native Audio Mixer" /
//! "Native Audio Bus Master") audio controller.
//!
//! Chosen as this project's third audio backend for the same reason as the
//! first two: it is directly relevant to real hardware this project
//! actually runs on (the Intel ICH4 south bridge in the P4 machine used
//! throughout AGENTS.md's real-hardware sections has exactly this
//! controller, PCI ID `8086:24d5`, discovered via the `pci` shell command
//! -- see AGENTS.md's AC'97 section), and it is also QEMU's own emulated
//! sound device (`-device AC97`, PCI ID `8086:2415`, Intel's original
//! `82801AA` ICH AC'97 controller), so this driver gets real `make test`
//! coverage without needing physical hardware for every iteration. Unlike
//! ES1370, which is a single self-contained chip owning its own DAC/ADC
//! and (on real hardware) an external mixer chip, AC'97 splits into two
//! genuinely separate pieces reachable through two separate I/O-space
//! BARs: the "Native Audio Mixer" (NAM, BAR0) is a thin indirect-register
//! window onto an external AC'97 *codec* chip living on its own AC-link
//! serial bus (volume/mute/record-source controls), while the "Native
//! Audio Bus Master" (NABM, BAR1) is the PCI-side DMA engine (one
//! independent bus-master channel each for PCM-in, PCM-out, and mic-in,
//! each with its own 32-entry ring of buffer descriptors) that actually
//! moves samples to/from memory. This driver only programs PCM-out
//! (playback); PCM-in and mic-in are left untouched, matching this
//! project's "smallest working increment first" convention (ES1370's own
//! ADC support was added in a later, separate increment).
//!
//! Every register offset and bit definition below was cross-checked
//! directly against QEMU's own `hw/audio/ac97.c`/`hw/audio/ac97.h`
//! emulation source (fetched and read line-by-line while writing this
//! driver, not reconstructed from memory) -- the same rigor this project
//! applies to every other register-level driver after the RTL8139 lessons
//! (see AGENTS.md's "Real hardware notes"). It is, as of this writing,
//! **unverified against real hardware** for the actual audio-output path
//! (see AGENTS.md's AC'97 section for exactly what has and hasn't been
//! confirmed on the real ICH4 machine).
//!
//! **The buffer-descriptor ring genuinely halts if software doesn't keep
//! feeding it, unlike ES1370's simpler two-register scheme.** Each
//! bus-master channel has a fixed, hardware-defined 32-entry ring
//! (`BDL_ENTRIES`); a `CIV` (Current Index Value) register tracks which
//! entry is currently playing, and an `LVI` (Last Valid Index) register --
//! written by software -- marks how far into the ring hardware is allowed
//! to advance. Confirmed directly in QEMU's own emulation
//! (`nabm_writeb`'s `PI_CR`/`PO_CR`/`MC_CR` case): the instant `CIV`
//! reaches `LVI`, hardware sets the `SR_DCH` ("DMA Controller Halted")
//! status bit and stops, full stop, regardless of how many *other* ring
//! entries still hold valid buffer descriptors -- there is no automatic
//! wraparound. A first draft of this driver assumed writing `LVI` once
//! (e.g. to the ring's last index) and never touching it again would give
//! a permanently looping ring "for free"; tracing QEMU's own
//! `nabm_writeb`/`fetch_bd` logic disproved that before any code was
//! written against it. The actual, standard technique (matching real
//! ICH AC'97 drivers): keep only `PERIOD_COUNT` (2) physical buffers, but
//! populate all `BDL_ENTRIES` (32) descriptor slots to reuse them
//! cyclically (`slot i` points at physical half `i % PERIOD_COUNT`), and
//! on every completion interrupt, refill the half that just finished
//! *then* advance `LVI` by exactly one more slot -- always keeping
//! exactly one slot of margin between `CIV` and `LVI`, the same
//! ISR-driven refill discipline `es1370.rs`'s own double-buffered ring
//! already uses, just expressed as a sliding window over a much larger
//! fixed-size ring instead of a hardware register pair. Writing `LVI`
//! while the ring is not halted merely extends the horizon (QEMU's own
//! `nabm_writeb` only performs the "kick" side effect -- `CIV = PIV`,
//! fetch the next descriptor, clear `SR_DCH` -- when `CR_RPBM` is set
//! *and* `SR_DCH` is currently set), so this refill/extend step has no
//! unwanted side effects during steady-state playback; it also means a
//! late refill that lets the ring genuinely halt is self-healing the
//! next time this driver writes `LVI` again, with no special-cased
//! underrun-recovery path needed.
//!
//! **Deliberately not implemented, as known real-hardware gaps rather
//! than oversights:** Variable Rate Audio (this driver always runs the
//! fixed, VRA-less 48 kHz rate every AC'97 codec must support without
//! negotiation, matching ES1370's own "one fixed hardware rate, no
//! runtime negotiation" convention -- see `audio.rs`'s own doc comment on
//! why the facade never assumes a card can be steered to an arbitrary
//! rate); the Codec Access Semaphore (`CAS`, NABM offset `0x34`) real ICH
//! hardware documents polling before each NAM mixer write -- QEMU's own
//! model does not actually gate anything on it (`nabm_readb`'s `CAS` case
//! just returns the current value and unconditionally sets it, with no
//! corresponding wait anywhere else in that file), and guessing at the
//! real protocol's exact clear condition risked a busy-loop hang on real
//! hardware for a case QEMU cannot exercise at all -- left out rather than
//! guessed at, exactly as ES1370's own doc comment leaves out its external
//! AK4531 mixer chip; and PCM-in/mic-in (playback only, this increment).

use core::cell::Cell;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch::x86::{inb, inl, inw, outb, outl, outw};
use crate::audio;
use crate::interrupts;
use crate::pci;
use crate::pmem;

const VENDOR_INTEL: u16 = 0x8086;
/// Intel's original `82801AA` ICH AC'97 controller -- QEMU's own emulated
/// `-device AC97` model (`PCI_DEVICE_ID_INTEL_82801AA_5` in QEMU's
/// `include/hw/pci/pci_ids.h`). Included specifically so this driver has
/// real automated test coverage, the same reasoning `es1370.rs` documents
/// for choosing ES1370.
const DEVICE_ICH_82801AA: u16 = 0x2415;
/// The Intel ICH4 south bridge's own AC'97 controller, confirmed present
/// via the `pci` shell command on the real P4 machine this driver was
/// motivated by (see AGENTS.md's AC'97 section). Every ICH-generation
/// AC'97 controller (ICH0 through at least ICH6) shares this exact
/// NAM/NABM register layout by design -- Intel's own explicit
/// backward-compatibility goal for this interface -- so extending
/// coverage to this specific, confirmed real device alongside QEMU's own
/// ID is a genuinely safe pairing, not a speculative broadening across
/// untested silicon the way a NIC family's sibling device IDs would be
/// (see `i350.rs`'s own, much more cautious one-ID-at-a-time discipline
/// for the contrasting case where sibling IDs really do differ).
const DEVICE_ICH4: u16 = 0x24d5;

/// Fallback I/O port bases used only when a BAR reads back completely
/// blank and this driver has to assign one itself -- see
/// `pci::PciAddress::assign_io_bar`'s own doc comment for the mechanism,
/// and why this is a guess, not a verified-free address. Confirmed
/// necessary on the real ICH4 machine this driver was built for: its
/// BIOS leaves both AC'97 BARs at `0`, never assigning them at all.
/// Chosen deliberately far above every I/O range this project has ever
/// observed a real chipset actually use for anything else -- the same
/// machine's own UHCI controllers, the highest other I/O BARs seen on
/// it, sit at `0xcc00`-`0xd800` (see AGENTS.md's UHCI section) -- while
/// staying spaced far enough apart from each other that neither
/// controller's own (typically 64-256 byte) decode window can overlap
/// the other's.
const NAM_FALLBACK_BASE: u16 = 0xe000;
const NABM_FALLBACK_BASE: u16 = 0xe100;

// NABM (Native Audio Bus Master, BAR1) register offsets. `MKREGS` in
// QEMU's own `ac97.c` derives each bus-master channel's own six
// sub-registers from a `channel_index * 16` base; only PCM-out ("PO",
// channel index 1) is used here.
const PO_BASE: u16 = 0x10;
const REG_PO_BDBAR: u16 = PO_BASE; // u32, rw
const REG_PO_LVI: u16 = PO_BASE + 0x05; // u8, rw
const REG_PO_SR: u16 = PO_BASE + 0x06; // u16, rw (write-1-to-clear bits)
const REG_PO_CR: u16 = PO_BASE + 0x0b; // u8, rw

const REG_GLOB_CNT: u16 = 0x2c; // u32
const REG_GLOB_STA: u16 = 0x30; // u32

/// `GLOB_CNT`'s Cold Reset bit (`GC_CR` in QEMU's `ac97.c`): clearing it
/// asserts a reset of the entire AC-link (including the external codec);
/// setting it again releases the reset, after which the codec needs a
/// real settle interval before it reports itself ready.
const GLOB_CNT_COLD_RESET: u32 = 1 << 1;
/// `GLOB_STA`'s "primary codec ready" bit (`GS_S0CR`), polled after a
/// cold reset (or checked first, in case the codec was already left
/// running from a previous boot/reset cycle -- QEMU's own model always
/// reports this bit set regardless of reset state, so that path is
/// exercised only on real hardware).
const GLOB_STA_PRIMARY_CODEC_READY: u32 = 1 << 8;

/// `SR`'s "Buffer Completion Interrupt Status" bit: set once the buffer
/// descriptor with `BD_IOC` set finishes playing. Write-1-to-clear.
const SR_BCIS: u16 = 1 << 3;
/// `SR`'s "FIFO Error" bit, also write-1-to-clear. A real, observed
/// condition under QEMU's own emulation with no host audio backend
/// attached (the mixer runs unthrottled, faster than any real DAC clock,
/// so the fixed-size ring underruns almost immediately) -- **leaving
/// this bit unacknowledged left the shared IRQ11 line (also used by the
/// e1000 NIC in the same test configuration) permanently asserted,
/// hanging the entire machine the instant playback started**, since an
/// unacknowledged status bit re-fires the interrupt the moment `iret`
/// restores `EFLAGS.IF`, exactly the ES1370 interrupt-storm lesson this
/// project already learned once (see `es1370.rs`'s own
/// `acknowledge_dac1` doc comment). A prior, more size-conscious draft of
/// this driver acknowledged only `SR_BCIS`, on the theory that never
/// setting `CR_FEIE` (this bit's own enable) meant it could never
/// actually assert the shared line -- confirmed wrong by this exact
/// hang, and fixed by acknowledging every bit in `SR_WCLEAR_MASK`
/// unconditionally, regardless of which one QEMU's own emulation
/// happens to gate the interrupt on.
const SR_FIFOE: u16 = 1 << 4;
/// Every write-1-to-clear bit in `SR` this driver acknowledges on every
/// interrupt, not just whichever one triggered period-refill logic --
/// see `SR_FIFOE`'s own doc comment for why acknowledging only `SR_BCIS`
/// caused a real hang.
const SR_WCLEAR_MASK: u16 = SR_BCIS | SR_FIFOE;

/// `CR`'s "Run/Pause Bus Master" bit: the actual playback start/stop
/// switch. Writing this while the ring is halted (`SR_DCH` set) is what
/// triggers hardware to fetch the descriptor at `PIV` and begin playing.
const CR_RPBM: u8 = 1 << 0;
/// `CR`'s self-clearing "Reset Registers" bit, used once at bring-up to
/// force this channel's `CIV`/`LVI`/`SR`/`PIV` back to a known state
/// regardless of whatever a previous boot (or a previous `usb`-style
/// UHCI-adjacent reset elsewhere on this PCI bus) left them in.
const CR_RR: u8 = 1 << 1;
/// `CR`'s "FIFO Error Interrupt Enable" bit -- see `SR_FIFOE`'s own doc
/// comment for why this is enabled despite this driver having no
/// separate FIFO-error counter yet.
const CR_FEIE: u8 = 1 << 3;
/// `CR`'s "Interrupt On Completion Enable" bit: without this set, a
/// descriptor's own `BD_IOC` bit never actually raises `SR_BCIS`.
const CR_IOCE: u8 = 1 << 4;

/// Set in a buffer descriptor's second dword (bits 31:16 hold control
/// flags; bits 15:0 hold the buffer length in *samples*, i.e. 16-bit
/// words -- one stereo frame is 2 samples) to request an `SR_BCIS`
/// interrupt once that descriptor finishes.
const BD_IOC: u32 = 1 << 31;

// NAM (Native Audio Mixer, BAR0) mixer register indices.
const NAM_MASTER_VOLUME: u16 = 0x02;
const NAM_PCM_OUT_VOLUME: u16 = 0x18;

/// The AC'97 spec's fixed bus rate absent Variable Rate Audio -- see this
/// module's own doc comment for why VRA negotiation is out of scope.
const FIXED_SAMPLE_RATE: u32 = 48_000;
const BYTES_PER_FRAME: u32 = 4; // 16-bit stereo.

/// One playback period, in sample-frames -- matching `es1370.rs`'s own
/// `PERIOD_FRAMES` exactly (roughly 21 ms at 48 kHz here, close to that
/// driver's ~23 ms at 44.1 kHz), for the same reasoning: generous enough
/// to survive real ISR-scheduling jitter without being needlessly large.
const PERIOD_FRAMES: u32 = 1024;
const PERIOD_SAMPLES: u32 = PERIOD_FRAMES * 2;
/// How many of the two physical buffer halves this driver keeps -- see
/// this module's own doc comment for why this is unrelated to
/// `BDL_ENTRIES` (the hardware ring's own fixed slot count).
const PERIOD_COUNT: u32 = 2;
const PERIOD_BYTES: u32 = PERIOD_FRAMES * BYTES_PER_FRAME;
const BUFFER_BYTES: u32 = PERIOD_BYTES * PERIOD_COUNT;

/// The bus-master ring's own fixed hardware size (`CIV`/`LVI`/`PIV` are
/// all 5-bit fields, `% 32` throughout QEMU's own emulation) -- not a
/// tunable constant, a hardware fact.
const BDL_ENTRIES: usize = 32;
const BDL_BYTES: usize = BDL_ENTRIES * 8;

fn read_reg32(io_base: u16, reg: u16) -> u32 {
    unsafe { inl(io_base + reg) }
}

fn write_reg32(io_base: u16, reg: u16, value: u32) {
    unsafe { outl(io_base + reg, value) };
}

fn read_reg16(io_base: u16, reg: u16) -> u16 {
    unsafe { inw(io_base + reg) }
}

fn write_reg16(io_base: u16, reg: u16, value: u16) {
    unsafe { outw(io_base + reg, value) };
}

fn read_reg8(io_base: u16, reg: u16) -> u8 {
    unsafe { inb(io_base + reg) }
}

fn write_reg8(io_base: u16, reg: u16, value: u8) {
    unsafe { outb(io_base + reg, value) };
}

/// Bounded busy-poll attempt count, mirroring `hda.rs`'s own
/// `POLL_ATTEMPTS`: a fixed iteration count rather than a PIT-tick
/// deadline, since each iteration here is a single cheap port I/O read
/// with no reason to expect it needs millisecond-scale spacing. Used
/// directly at each poll site below rather than through a shared generic
/// helper, so only one non-generic loop body needs to exist per site.
const POLL_ATTEMPTS: u32 = 100_000;

/// How long to hold the AC-link in cold reset before releasing it --
/// mirroring `hda.rs`'s own `CODEC_ENUMERATION_WAIT_TICKS` settle-window
/// convention (a real codec needs a genuine minimum assertion time, not
/// just a same-tick clear-then-set).
const COLD_RESET_ASSERT_TICKS: u64 = 2;

#[derive(Debug, Clone, Copy)]
pub enum Status {
    NotPresent,
    InitFailed,
}

/// How many independent AC'97 controllers this driver will discover and
/// drive simultaneously. Unlike ES1370 (a discrete sound card a real
/// machine could plausibly have several of), AC'97 is always exactly one
/// fixed south-bridge function per motherboard -- there is no realistic
/// multi-AC'97 scenario the way an add-in Sound Blaster PCI card has, and
/// `audio::RING_CAPACITY_BYTES`'s own doc comment already documents how
/// little `.bss` headroom this kernel has to spend on a second instance
/// nothing would ever actually populate.
pub const MAX_INSTANCES: usize = 1;

pub struct Ac97 {
    nabm_base: u16,
    irq: Option<u8>,
    buffer_addr: u32,
    /// The next `LVI` value this driver will write, tracked in software
    /// rather than read back from hardware -- this driver is the ring's
    /// sole writer, so a local copy is exactly as accurate and far
    /// cheaper than a port read every interrupt. See this module's own
    /// doc comment for the sliding-window technique this drives.
    next_lvi: Cell<u8>,
    tone_phase: Cell<u32>,
    interrupt_count: AtomicU32,
    periods_serviced: AtomicU32,
}

static mut CARDS: [Option<Ac97>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CARD_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

impl Ac97 {
    pub fn format(&self) -> audio::AudioFormat {
        audio::AudioFormat {
            sample_rate: FIXED_SAMPLE_RATE,
            channels: 2,
            bits_per_sample: 16,
        }
    }

    /// No network-fed playback yet -- this increment only proves the
    /// controller initializes and plays its own built-in test tone (see
    /// `fill_period_tone`). A per-instance `audio::PcmRing` costs a fixed
    /// 16 KiB of `.bss` (`audio::RING_CAPACITY_BYTES`'s own doc comment),
    /// which this kernel did not have spare room for alongside a brand
    /// new PCI driver -- see AGENTS.md's AC'97 section for the exact
    /// low-memory-region budget this deferred. Always reports zero bytes
    /// accepted, the same "nothing accepted yet" signal a caller already
    /// has to handle for a temporarily-full ring on another backend.
    pub fn submit(&self, _data: &[u8]) -> usize {
        0
    }

    pub fn underrun_count(&self) -> u32 {
        0
    }

    pub fn ring_backlog(&self) -> usize {
        0
    }

    fn fill_period(&self, half: u32) {
        let offset = half * PERIOD_BYTES;
        let base = (self.buffer_addr + offset) as *mut u8;

        self.fill_period_tone(base);
    }

    /// A fixed, continuous test tone, identical in spirit to
    /// `es1370::Es1370::fill_period_tone` -- see that function's own doc
    /// comment. `CYCLE_FRAMES` is recomputed for this device's own fixed
    /// 48 kHz rate rather than reusing ES1370's 44.1 kHz constant.
    fn fill_period_tone(&self, base: *mut u8) {
        const CYCLE_FRAMES: u32 = 55; // 48000 / 55 / 2 ~= 436 Hz.
        const AMPLITUDE: i16 = 8000;

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
                core::ptr::write_volatile(sample_addr, bytes[0]);
                core::ptr::write_volatile(sample_addr.add(1), bytes[1]);
                core::ptr::write_volatile(sample_addr.add(2), bytes[0]);
                core::ptr::write_volatile(sample_addr.add(3), bytes[1]);
            }
            n = n.wrapping_add(1);
        }
        self.tone_phase.set(n);
    }

    /// Called from this driver's PCI IRQ handler once per elapsed
    /// playback period. See this module's own doc comment for why
    /// refilling the just-finished half *then* extending `LVI` by
    /// exactly one slot is both necessary (the ring genuinely halts
    /// otherwise) and sufficient (steady state keeps exactly one slot of
    /// margin between `CIV` and `LVI` forever).
    pub fn handle_interrupt(&self) {
        let status = read_reg16(self.nabm_base, REG_PO_SR);
        if status & SR_WCLEAR_MASK == 0 {
            return;
        }
        if status & SR_BCIS != 0 {
            self.interrupt_count.fetch_add(1, Ordering::Relaxed);
            let served = self.periods_serviced.fetch_add(1, Ordering::Relaxed);
            self.fill_period(served % PERIOD_COUNT);
            let new_lvi = self.next_lvi.get().wrapping_add(1) % (BDL_ENTRIES as u8);
            write_reg8(self.nabm_base, REG_PO_LVI, new_lvi);
            self.next_lvi.set(new_lvi);
        }
        // Every write-1-to-clear bit currently set is acknowledged here,
        // not only `SR_BCIS` -- see `SR_FIFOE`'s own doc comment for why
        // acknowledging just one of them left the shared IRQ line
        // permanently asserted.
        write_reg16(self.nabm_base, REG_PO_SR, status & SR_WCLEAR_MASK);
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32) {
        (
            self.irq,
            self.interrupt_count.load(Ordering::Relaxed),
            self.periods_serviced.load(Ordering::Relaxed),
        )
    }

    /// This device's base address for diagnostics -- see `Audio::io_base`'s
    /// own doc comment for why this is widened to `u32`. Reports the NABM
    /// (bus-master/DMA) base, the address space this driver's own
    /// interrupt handling and playback state actually live in, matching
    /// how `es1370.rs` reports its one and only I/O base.
    pub fn io_base(&self) -> u32 {
        self.nabm_base as u32
    }

    pub fn raw_status(&self) -> u32 {
        read_reg16(self.nabm_base, REG_PO_SR) as u32
    }
}

/// Shared legacy IRQs are normal -- see `es1370.rs`'s own doc comment on
/// this same point.
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

/// Discovers and starts every usable AC'97 controller (up to
/// `MAX_INSTANCES`). Idempotent, mirroring `es1370::initialize_all`.
pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if INITIALIZED {
            return if CARD_COUNT == 0 { Err(Status::NotPresent) } else { Ok(CARD_COUNT) };
        }
        INITIALIZED = true;
    }

    let mut matches: [Option<pci::PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_devices(
        VENDOR_INTEL,
        &[DEVICE_ICH_82801AA, DEVICE_ICH4],
        &mut matches,
    );
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
            if init_failed { Err(Status::InitFailed) } else { Err(Status::NotPresent) }
        } else {
            Ok(CARD_COUNT)
        }
    }
}

/// Forces (or confirms) the AC-link's cold reset, then polls for the
/// primary codec to report itself ready. See `GLOB_STA_PRIMARY_CODEC_READY`'s
/// own doc comment for why the already-ready check comes first.
fn cold_reset_and_wait_ready(nabm_base: u16) -> Result<(), Status> {
    if read_reg32(nabm_base, REG_GLOB_STA) & GLOB_STA_PRIMARY_CODEC_READY != 0 {
        return Ok(());
    }

    let cnt = read_reg32(nabm_base, REG_GLOB_CNT);
    write_reg32(nabm_base, REG_GLOB_CNT, cnt & !GLOB_CNT_COLD_RESET);
    let deadline = interrupts::ticks().wrapping_add(COLD_RESET_ASSERT_TICKS);
    while interrupts::ticks() < deadline {}
    write_reg32(nabm_base, REG_GLOB_CNT, cnt | GLOB_CNT_COLD_RESET);

    for _ in 0..POLL_ATTEMPTS {
        if read_reg32(nabm_base, REG_GLOB_STA) & GLOB_STA_PRIMARY_CODEC_READY != 0 {
            return Ok(());
        }
    }
    Err(Status::InitFailed)
}

/// Reads `index`'s I/O BAR the ordinary way first, and only falls back to
/// `PciAddress::assign_io_bar` (a real, non-hypothetical need -- see
/// `NAM_FALLBACK_BASE`'s own doc comment) if firmware left it genuinely
/// blank.
fn ensure_io_bar(address: &pci::PciAddress, index: u8, fallback_base: u16) -> Option<u16> {
    if let Some(base) = address.io_bar(index) {
        return Some(base);
    }
    address.assign_io_bar(index, fallback_base)
}

/// Brings up one already-PCI-discovered controller and, on success,
/// appends it to `CARDS`. Split out of `initialize_all` purely so that
/// function's own per-address loop reads as a plain list of attempts, one
/// per discovered controller, mirroring `es1370::init_card`.
fn init_card(address: pci::PciAddress) -> Result<(), Status> {
    let instance = unsafe { CARD_COUNT };
    if instance >= MAX_INSTANCES {
        return Err(Status::InitFailed);
    }

    // If either BAR turns out unassignable, print the raw probe/power
    // data behind that failure -- confirmed genuinely necessary on real
    // hardware: the P4/ICH4 machine this driver targets has *both* BARs
    // read back as a hard `0` even after the standard all-1s sizing
    // probe, in full D0 power (ruling out a power-managed/disabled-
    // device explanation). Per the PCI spec, an I/O BAR's own type bit
    // is hardwired and always readable regardless of size, so this means
    // there is no register storage behind either BAR at all -- not a
    // firmware oversight `assign_io_bar` could fix, but this specific
    // board's ICH4 AC'97 function most likely having no physical codec
    // populated at all. See AGENTS.md's AC'97 section.
    let nam_result = ensure_io_bar(&address, 0, NAM_FALLBACK_BASE);
    let nabm_result = ensure_io_bar(&address, 1, NABM_FALLBACK_BASE);
    if nam_result.is_none() || nabm_result.is_none() {
        crate::println!(
            "ac97: BAR0 raw={:?} BAR1 raw={:?} power_state={:?}",
            address.debug_probe_bar(0),
            address.debug_probe_bar(1),
            address.debug_power_state()
        );
    }
    let Some(nam_base) = nam_result else {
        crate::println!("ac97: BAR0 (NAM) has no usable I/O address (blank, and this driver's own fallback assignment failed too); cannot proceed.");
        return Err(Status::InitFailed);
    };
    let Some(nabm_base) = nabm_result else {
        crate::println!("ac97: BAR1 (NABM) has no usable I/O address (blank, and this driver's own fallback assignment failed too); cannot proceed.");
        return Err(Status::InitFailed);
    };
    address.enable_io_and_bus_master();
    let irq = address.interrupt_line();

    if let Err(status) = cold_reset_and_wait_ready(nabm_base) {
        crate::println!(
            "ac97: cold reset timed out; GLOB_STA=0x{:08x} GLOB_CNT=0x{:08x}",
            read_reg32(nabm_base, REG_GLOB_STA),
            read_reg32(nabm_base, REG_GLOB_CNT)
        );
        return Err(status);
    }

    // Unmute and set 0 dB attenuation on both the master output and the
    // PCM-out mixer path -- an AC'97 codec's power-on-reset defaults
    // commonly leave one or both muted (`MUTE_SHIFT`, bit 15, in QEMU's
    // own `ac97.h`), and this driver programs no external mixer chip
    // beyond this, matching `es1370.rs`'s own documented gap for its
    // external AK4531.
    write_reg16(nam_base, NAM_MASTER_VOLUME, 0x0000);
    write_reg16(nam_base, NAM_PCM_OUT_VOLUME, 0x0000);

    // Force this channel's own bus-master registers (CIV/LVI/SR/PIV) to
    // their reset state regardless of whatever a previous boot left them
    // in -- `CR_RR` is self-clearing.
    write_reg8(nabm_base, REG_PO_CR, CR_RR);
    let mut rr_cleared = false;
    for _ in 0..POLL_ATTEMPTS {
        if read_reg8(nabm_base, REG_PO_CR) & CR_RR == 0 {
            rr_cleared = true;
            break;
        }
    }
    if !rr_cleared {
        crate::println!(
            "ac97: PO_CR reset-registers bit never cleared; CR=0x{:02x}",
            read_reg8(nabm_base, REG_PO_CR)
        );
        return Err(Status::InitFailed);
    }

    let buffer_frames = (BUFFER_BYTES as usize).div_ceil(pmem::FRAME_SIZE as usize);
    let Some(buffer_addr) = pmem::allocate_contiguous(buffer_frames) else {
        crate::println!("ac97: failed to allocate the playback DMA buffer.");
        return Err(Status::InitFailed);
    };
    let Some(bdl_addr) = pmem::allocate() else {
        crate::println!("ac97: failed to allocate the BDL table frame.");
        return Err(Status::InitFailed);
    };

    // Every one of the ring's 32 fixed slots is populated up front,
    // cyclically reusing the two physical halves above -- see this
    // module's own doc comment for why this, plus an `LVI` that keeps
    // sliding forward one slot per interrupt, is what makes the ring
    // loop indefinitely rather than halting after two periods.
    unsafe {
        core::ptr::write_bytes(bdl_addr as *mut u8, 0, BDL_BYTES);
        let bdl_ptr = bdl_addr as *mut u32;
        for i in 0..BDL_ENTRIES {
            let half = (i as u32) % PERIOD_COUNT;
            let addr = buffer_addr + half * PERIOD_BYTES;
            core::ptr::write_volatile(bdl_ptr.add(i * 2), addr);
            core::ptr::write_volatile(bdl_ptr.add(i * 2 + 1), PERIOD_SAMPLES | BD_IOC);
        }
    }

    let card = Ac97 {
        nabm_base,
        irq,
        buffer_addr,
        next_lvi: Cell::new(1),
        tone_phase: Cell::new(0),
        interrupt_count: AtomicU32::new(0),
        periods_serviced: AtomicU32::new(0),
    };

    card.fill_period(0);
    card.fill_period(1);

    write_reg32(nabm_base, REG_PO_BDBAR, bdl_addr);
    // CR_RPBM is still 0 at this point, so writing LVI here only records
    // the value -- QEMU's own `nabm_writeb` only performs the "kick" side
    // effect when CR_RPBM is already set, which the very next write below
    // is what actually sets.
    write_reg8(nabm_base, REG_PO_LVI, 1);

    if let Some(irq) = irq {
        if !interrupts::register_pci_irq(irq, handle_pci_irq) {
            crate::println!("ac97: failed to register the legacy PCI IRQ handler.");
            return Err(Status::InitFailed);
        }
    }

    write_reg8(nabm_base, REG_PO_CR, CR_RPBM | CR_IOCE | CR_FEIE);

    unsafe {
        CARDS[instance] = Some(card);
        CARD_COUNT += 1;
    }
    Ok(())
}

/// Returns one already-discovered instance by index. Assumes
/// `initialize_all` has already run; callers reach this only through
/// `audio.rs`'s own facade.
pub fn get(index: usize) -> Option<&'static Ac97> {
    unsafe {
        if index >= CARD_COUNT {
            return None;
        }
        CARDS[index].as_ref()
    }
}
