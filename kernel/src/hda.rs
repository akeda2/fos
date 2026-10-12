//! Intel High Definition Audio ("HD Audio", HDA): the onboard sound of nearly every PC since 2004.
//!
//! HDA is two chips talking over a private link, and this file drives the first and talks to the second.
//!
//! * **The controller** is a PCI function with one 16 KiB register window. It owns the DMA engines: a pair of
//!   rings for *commands* (CORB, host to codec) and *responses* (RIRB, codec to host), and up to 30 *stream
//!   descriptors*, each of which copies PCM between a host buffer and the link by DMA.
//! * **The codec** (a Realtek ALC, an IDT, a Conexant, a VIA ... or a GPU's HDMI audio) is a separate chip on the
//!   link, up to 15 of them, addressed `0..14`. What it can do is not fixed by any ID: it is a graph of *widgets*
//!   (nodes, `nid`) that the driver must read out with 32-bit *verbs*: converters (DAC = output, ADC = input),
//!   mixers and selectors, and pin complexes (the jacks), each with a list of the widgets that feed it.
//!
//! So where the NIC drivers know their register shape from the PCI ID, this driver *discovers* it:
//!
//! 1. **Reset** the controller (`GCTL.CRST`), wait for the codecs to announce themselves in `STATESTS`.
//! 2. **CORB/RIRB**: allocate the two rings, program their bases and sizes, start both DMA engines. A verb is
//!    one `u32` written into the CORB; its response is one entry the controller writes into the RIRB.
//! 3. **Walk the graph**: root node -> Audio Function Group -> every widget: type and capabilities
//!    (`AUDIO_WIDGET_CAP`), for converters their PCM rates and sample sizes, for pins their capabilities and
//!    *default configuration* (which jack, what colour, whether anything is wired to it at all), for everything
//!    with inputs the connection list.
//! 4. **Path search**: a widget's connection list names the widgets that can feed it, so a playback path is
//!    found by starting at an output pin and following connections back through mixers/selectors to a DAC, and a
//!    capture path by starting at an ADC and following connections down to an input pin. All paths are scored by
//!    what the pin is (line out beats a speaker, line in beats a microphone, "no physical connection" is skipped)
//!    and the best of each direction is kept.
//! 5. **Stream setup** (at `audio_start`): choose a sample rate/size the converter offers, program a stream
//!    descriptor's buffer descriptor list (BDL) and format, tell the converter its stream tag and format, unmute
//!    every amplifier on the path, select the connections, power the widgets, enable the pin, and start the DMA.
//!
//! **Streaming.** A stream's buffer is a ring of `PERIODS` periods (`PERIOD_FRAMES` frames each); every BDL entry
//! asks for an interrupt when the controller has finished it. The interrupt handler asks the controller where it
//! is (the *DMA position buffer* the controller updates in host memory, or the `LPIB` register if that does not
//! work), and services every period it has completed since last time: playback pops PCM from a ring into the period
//! (silence when the ring runs dry: an underrun), capture pushes the period into a ring. Servicing "everything
//! between where we were and where the hardware is" rather than "one period per interrupt" means a late or merged
//! interrupt cannot make the driver lose count. The rings are the same `PcmRing` the USB audio driver uses, so a
//! tape sees one behaviour (`audio_write` returns how much fitted; `audio_read` returns what has been captured).
//!
//! **Interrupts**: MSI if the controller has it and a self-test message arrives (the test sends one verb with the
//! RIRB "response interrupt" enabled, which needs no stream), else legacy INTx through the I/O APIC by probing
//! candidate pins as the NIC drivers do, else polled: the foreground then services the streams from `poll()`
//! (called by every `audio_read`/`audio_write`). `no-msi` in the settings file means polled.
//!
//! **DMA addresses**: `GCAP.64OK` says whether the controller can address memory above 4 GiB. If it cannot, every
//! buffer (CORB, RIRB, BDL, position buffer, the audio buffers) is allocated below 4 GiB.
//!
//! Ported from v1's `kernel/src/hda.rs` (QEMU-verified there), with what v1 lacked and real codecs need: a real path
//! search through mixers and selectors, amplifier unmuting along the path, pin-configuration based jack choice, power
//! states, EAPD, pin VREF for microphones, the full PCM rate table, 2/16/256-entry CORB/RIRB, the position buffer,
//! and teardown. **Everything beyond what QEMU's synthetic codecs exercise is unverified on real hardware.**

use crate::audio::PcmRing;
use crate::{apic, cpu, memory, msi, paging, pci};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

pub const MAX_CONTROLLERS: usize = 4;
const MAX_CODECS: usize = 4;
/// Widgets kept per codec (a Realtek ALC has 30-80). More are counted but not kept.
const MAX_WIDGETS: usize = 128;
/// Connection-list entries kept per widget.
const MAX_CONNS: usize = 16;
/// A path is at most this many widgets (pin, a few mixers/selectors, converter).
const MAX_PATH: usize = 8;

/// The register window (QEMU's `intel-hda` and real controllers: 16 KiB).
const MMIO_BYTES: u64 = 0x4000;

// ---- controller registers (HDA specification 1.0a, section 3.3) ----
const REG_GCAP: u64 = 0x00;
const REG_VMIN: u64 = 0x02;
const REG_GCTL: u64 = 0x08;
const REG_STATESTS: u64 = 0x0e;
const REG_INTCTL: u64 = 0x20;
const REG_INTSTS: u64 = 0x24;
const REG_CORBLBASE: u64 = 0x40;
const REG_CORBUBASE: u64 = 0x44;
const REG_CORBWP: u64 = 0x48;
const REG_CORBRP: u64 = 0x4a;
const REG_CORBCTL: u64 = 0x4c;
const REG_CORBSIZE: u64 = 0x4e;
const REG_RIRBLBASE: u64 = 0x50;
const REG_RIRBUBASE: u64 = 0x54;
const REG_RIRBWP: u64 = 0x58;
const REG_RINTCNT: u64 = 0x5a;
const REG_RIRBCTL: u64 = 0x5c;
const REG_RIRBSTS: u64 = 0x5d;
const REG_RIRBSIZE: u64 = 0x5e;
const REG_DPLBASE: u64 = 0x70;
const REG_DPUBASE: u64 = 0x74;

const GCTL_CRST: u32 = 1;
const INTCTL_GIE: u32 = 1 << 31;
const INTCTL_CIE: u32 = 1 << 30;
const INTSTS_CIS: u32 = 1 << 30;
/// `CORBRP` bit 15: set to reset the read pointer, then clear again (the controller confirms each step).
const CORBRP_RST: u16 = 1 << 15;
const CORBCTL_RUN: u8 = 1 << 1;
const RIRBWP_RST: u16 = 1 << 15;
const RIRBCTL_INT_EN: u8 = 1 << 0;
const RIRBCTL_DMA_EN: u8 = 1 << 1;
/// `RIRBSTS` bit 0 (response interrupt) and bit 2 (overrun): write 1 to clear.
const RIRBSTS_CLEAR: u8 = 0x05;

// ---- stream descriptor registers, at 0x80 + 0x20 * index (input streams first, then output) ----
const SD_BASE: u64 = 0x80;
const SD_STRIDE: u64 = 0x20;
const SD_CTL: u64 = 0x00;
const SD_STS: u64 = 0x03;
const SD_LPIB: u64 = 0x04;
const SD_CBL: u64 = 0x08;
const SD_LVI: u64 = 0x0c;
const SD_FMT: u64 = 0x12;
const SD_BDPL: u64 = 0x18;
const SD_BDPU: u64 = 0x1c;
const SD_CTL_SRST: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;
const SD_CTL_TAG_SHIFT: u32 = 20;
/// `SD_STS`: buffer completion (2), FIFO error (3), descriptor error (4). Write 1 to clear.
const SD_STS_BCIS: u8 = 1 << 2;
const SD_STS_ERRORS: u8 = (1 << 3) | (1 << 4);

// ---- verbs ----
const VERB_GET_PARAMETER: u32 = 0xf00;
const VERB_GET_CONNECT_LIST: u32 = 0xf02;
const VERB_GET_PIN_SENSE: u32 = 0xf09;
const VERB_GET_CONFIG_DEFAULT: u32 = 0xf1c;
const VERB_SET_CONNECT_SEL: u32 = 0x701;
const VERB_SET_POWER_STATE: u32 = 0x705;
const VERB_SET_CHANNEL_STREAMID: u32 = 0x706;
const VERB_SET_PIN_WIDGET_CONTROL: u32 = 0x707;
const VERB_SET_EAPD: u32 = 0x70c;
const VERB_SET_CVT_CHAN_COUNT: u32 = 0x72d;
/// The 4-bit-verb / 16-bit-payload verbs (`verb16`).
const VERB_SET_STREAM_FORMAT: u32 = 0x2;
const VERB_SET_AMP_GAIN_MUTE: u32 = 0x3;

const PAR_VENDOR_ID: u32 = 0x00;
const PAR_NODE_COUNT: u32 = 0x04;
const PAR_FUNCTION_TYPE: u32 = 0x05;
const PAR_WIDGET_CAP: u32 = 0x09;
const PAR_PCM: u32 = 0x0a;
const PAR_STREAMS: u32 = 0x0b;
const PAR_PIN_CAP: u32 = 0x0c;
const PAR_AMP_IN_CAP: u32 = 0x0d;
const PAR_CONN_LEN: u32 = 0x0e;
const PAR_AMP_OUT_CAP: u32 = 0x12;

// Widget capability bits and types.
const WCAP_STEREO: u32 = 1 << 0;
const WCAP_IN_AMP: u32 = 1 << 1;
const WCAP_OUT_AMP: u32 = 1 << 2;
const WCAP_AMP_OVERRIDE: u32 = 1 << 3;
const WCAP_FORMAT_OVERRIDE: u32 = 1 << 4;
const WCAP_CONN_LIST: u32 = 1 << 8;
const WCAP_DIGITAL: u32 = 1 << 9;
const WCAP_POWER: u32 = 1 << 10;
const KIND_OUTPUT: u8 = 0;
const KIND_INPUT: u8 = 1;
const KIND_MIXER: u8 = 2;
const KIND_SELECTOR: u8 = 3;
const KIND_PIN: u8 = 4;

// Pin capability bits.
const PINCAP_PRESENCE: u32 = 1 << 2;
const PINCAP_OUTPUT: u32 = 1 << 4;
const PINCAP_INPUT: u32 = 1 << 5;
const PINCAP_EAPD: u32 = 1 << 16;
const PINCAP_VREF80: u32 = 1 << 12;
// Pin control bits.
const PINCTL_HP_EN: u32 = 1 << 7;
const PINCTL_OUT_EN: u32 = 1 << 6;
const PINCTL_IN_EN: u32 = 1 << 5;
const PINCTL_VREF80: u32 = 4;

// Amplifier verb payload bits.
const AMP_OUTPUT: u32 = 1 << 15;
const AMP_INPUT: u32 = 1 << 14;
const AMP_LEFT: u32 = 1 << 13;
const AMP_RIGHT: u32 = 1 << 12;

/// Default devices of a pin's default configuration (bits 23:20).
const DEV_LINE_OUT: u32 = 0x0;
const DEV_SPEAKER: u32 = 0x1;
const DEV_HP_OUT: u32 = 0x2;
const DEV_CD: u32 = 0x3;
const DEV_LINE_IN: u32 = 0x8;
const DEV_AUX: u32 = 0x9;
const DEV_MIC_IN: u32 = 0xa;

/// Frames per period and periods per buffer. 512 frames is 10.7 ms at 48 kHz; eight of them give the foreground
/// about 85 ms of slack before a capture overruns or a playback runs dry.
const PERIOD_FRAMES: usize = 512;
const PERIODS: usize = 8;
const PLAYBACK_TAG: u32 = 1;
const CAPTURE_TAG: u32 = 2;

/// The sample rates `AUDIO PCM rates` bits 0-11 stand for: (rate, base 44.1 kHz instead of 48, multiplier, divisor).
/// The stream format word encodes a rate as base, multiplier-1 and divisor-1.
const RATES: [(u32, bool, u32, u32); 12] = [
    (8000, false, 1, 6),
    (11025, true, 1, 4),
    (16000, false, 1, 3),
    (22050, true, 1, 2),
    (32000, false, 2, 3),
    (44100, true, 1, 1),
    (48000, false, 1, 1),
    (88200, true, 2, 1),
    (96000, false, 2, 1),
    (176400, true, 4, 1),
    (192000, false, 4, 1),
    (384000, false, 8, 1),
];
/// The order rates are offered to a tape: the common ones first (the tape's `AudioInfo` holds only eight).
const RATE_ORDER: [u32; 12] = [48000, 44100, 96000, 88200, 192000, 176400, 32000, 22050, 16000, 11025, 8000, 384000];

#[derive(Clone, Copy)]
struct Widget {
    nid: u8,
    kind: u8,
    caps: u32,
    pin_caps: u32,
    /// The pin's default configuration (0 for other widgets, or when the codec does not say).
    config: u32,
    conn_count: u8,
    conns: [u8; MAX_CONNS],
    /// Converter: supported rates (bits 0-11) and sample sizes (bits 16-20), and whether it takes PCM.
    pcm: u32,
    amp_in: u32,
    amp_out: u32,
}

impl Widget {
    const EMPTY: Widget = Widget { nid: 0, kind: 0, caps: 0, pin_caps: 0, config: 0, conn_count: 0, conns: [0; MAX_CONNS], pcm: 0, amp_in: 0, amp_out: 0 };

    fn max_channels(&self) -> u8 {
        if self.caps & WCAP_STEREO == 0 {
            1
        } else {
            let extension = (self.caps >> 13) & 7;
            (((extension << 1) | 1) + 1) as u8
        }
    }
}

#[derive(Clone, Copy)]
struct Codec {
    present: bool,
    address: u8,
    vendor: u16,
    device: u16,
    afg: u8,
    first_nid: u8,
    /// Widgets in the AFG, and how many of them fit `widgets`.
    total: usize,
    count: usize,
    afg_pcm: u32,
    afg_amp_in: u32,
    afg_amp_out: u32,
    widgets: [Widget; MAX_WIDGETS],
}

impl Codec {
    const EMPTY: Codec = Codec {
        present: false,
        address: 0,
        vendor: 0,
        device: 0,
        afg: 0,
        first_nid: 0,
        total: 0,
        count: 0,
        afg_pcm: 0,
        afg_amp_in: 0,
        afg_amp_out: 0,
        widgets: [Widget::EMPTY; MAX_WIDGETS],
    };

    fn widget(&self, nid: u8) -> Option<&Widget> {
        let index = usize::from(nid.wrapping_sub(self.first_nid));
        (nid >= self.first_nid && index < self.count).then(|| &self.widgets[index])
    }
}

/// A route through a codec's widget graph. `nodes[0]` is the sink end: for playback the output pin, for capture the
/// ADC; `nodes[len-1]` is the source end (the DAC, or the input pin). `nodes[i + 1]` is entry `selects[i]` of
/// `nodes[i]`'s connection list.
#[derive(Clone, Copy)]
struct Path {
    valid: bool,
    codec: u8,
    len: u8,
    score: u8,
    nodes: [u8; MAX_PATH],
    selects: [u8; MAX_PATH],
}

impl Path {
    const NONE: Path = Path { valid: false, codec: 0, len: 0, score: 0, nodes: [0; MAX_PATH], selects: [0; MAX_PATH] };

    fn converter(&self, capture: bool) -> u8 {
        if capture { self.nodes[0] } else { self.nodes[usize::from(self.len) - 1] }
    }
    fn pin(&self, capture: bool) -> u8 {
        if capture { self.nodes[usize::from(self.len) - 1] } else { self.nodes[0] }
    }
}

/// How interrupts reach the driver.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Polled,
    Msi(u8),
    Intx { gsi: u32, vector: u8 },
}

/// One direction of audio in flight. Built completely by the foreground, published by setting `active` (with
/// interrupts off), serviced by the interrupt handler (or `poll`), unpublished the same way.
struct Stream {
    active: AtomicBool,
    index: u8,
    tag: u8,
    capture: bool,
    channels: u8,
    bits: u8,
    /// Bytes one sample takes in the buffer: 2 for 16-bit, 4 for 24-bit (HDA carries 24 bits left-justified in 32).
    container: u8,
    rate: u32,
    format: u16,
    buffer: u64,
    buffer_frames: u64,
    bdl: u64,
    period_bytes: usize,
    /// The period the next completion is expected for.
    next: usize,
    pcm: PcmRing,
    periods: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    underruns: AtomicU64,
    /// Playback: set once the tape has written anything, so the silence before and after it is not an underrun.
    primed: AtomicBool,
    path: Path,
}

impl Stream {
    const EMPTY: Stream = Stream {
        active: AtomicBool::new(false),
        index: 0,
        tag: 0,
        capture: false,
        channels: 0,
        bits: 0,
        container: 0,
        rate: 0,
        format: 0,
        buffer: 0,
        buffer_frames: 0,
        bdl: 0,
        period_bytes: 0,
        next: 0,
        pcm: PcmRing::EMPTY,
        periods: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
        primed: AtomicBool::new(false),
        path: Path::NONE,
    };

    fn frame_bytes(&self) -> usize {
        usize::from(self.channels) * usize::from(self.container)
    }
}

pub struct Hda {
    present: bool,
    pci: pci::Device,
    regs: u64,
    ok64: bool,
    version: (u8, u8),
    input_streams: u8,
    output_streams: u8,
    corb: u64,
    corb_entries: u16,
    rirb: u64,
    rirb_entries: u16,
    rirb_rp: u16,
    position_buffer: u64,
    position_buffer_works: bool,
    codecs: [Codec; MAX_CODECS],
    playback_path: Path,
    capture_path: Path,
    delivery: Delivery,
    interrupts: AtomicU64,
    playback: Stream,
    capture: Stream,
    /// The mode (index into `modes`) the user picked for [capture, playback]; 255 = automatic.
    picked: [u8; 2],
}

impl Hda {
    const EMPTY: Hda = Hda {
        present: false,
        pci: pci::Device { bus: 0, device: 0, function: 0, vendor: 0, id: 0, class: 0, subclass: 0, interface: 0, revision: 0 },
        regs: 0,
        ok64: false,
        version: (0, 0),
        input_streams: 0,
        output_streams: 0,
        corb: 0,
        corb_entries: 0,
        rirb: 0,
        rirb_entries: 0,
        rirb_rp: 0,
        position_buffer: 0,
        position_buffer_works: false,
        codecs: [Codec::EMPTY; MAX_CODECS],
        playback_path: Path::NONE,
        capture_path: Path::NONE,
        delivery: Delivery::Polled,
        interrupts: AtomicU64::new(0),
        playback: Stream::EMPTY,
        capture: Stream::EMPTY,
        picked: [255, 255],
    };
}

static mut CONTROLLERS: [Hda; MAX_CONTROLLERS] = [Hda::EMPTY; MAX_CONTROLLERS];
static mut COUNT: usize = 0;

fn controller(index: usize) -> &'static mut Hda {
    unsafe { &mut (*core::ptr::addr_of_mut!(CONTROLLERS))[index] }
}

/// How many controllers came up (some may have no usable path: see `usable`).
pub fn count() -> usize {
    unsafe { COUNT }
}

// ---- register access ----

fn read8(base: u64, offset: u64) -> u8 {
    unsafe { ((base + offset) as *const u8).read_volatile() }
}
fn read16(base: u64, offset: u64) -> u16 {
    unsafe { ((base + offset) as *const u16).read_volatile() }
}
fn read32(base: u64, offset: u64) -> u32 {
    unsafe { ((base + offset) as *const u32).read_volatile() }
}
fn write8(base: u64, offset: u64, value: u8) {
    unsafe { ((base + offset) as *mut u8).write_volatile(value) }
}
fn write16(base: u64, offset: u64, value: u16) {
    unsafe { ((base + offset) as *mut u16).write_volatile(value) }
}
fn write32(base: u64, offset: u64, value: u32) {
    unsafe { ((base + offset) as *mut u32).write_volatile(value) }
}

/// Polls `condition` for up to `microseconds`. Bounded by the clock and, in case the clock is not running, by an
/// iteration count: a stuck register must never hang the boot.
fn wait_until(microseconds: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = apic::micros();
    for _ in 0..20_000_000u32 {
        if condition() {
            return true;
        }
        if apic::micros().wrapping_sub(start) > microseconds {
            return condition();
        }
        core::hint::spin_loop();
    }
    false
}

/// A 12-bit verb with an 8-bit payload (every verb that fits: parameter reads, connection lists, selects, pin
/// control).
fn verb(cad: u8, nid: u8, id: u32, payload: u32) -> u32 {
    (u32::from(cad) << 28) | (u32::from(nid) << 20) | (id << 8) | (payload & 0xff)
}

/// A 4-bit verb with a 16-bit payload (stream format and amplifier gain/mute, whose payloads need the room).
fn verb16(cad: u8, nid: u8, id: u32, payload: u32) -> u32 {
    (u32::from(cad) << 28) | (u32::from(nid) << 20) | ((id & 0xf) << 16) | (payload & 0xffff)
}

impl Hda {
    fn dma_limit(&self) -> Option<u64> {
        if self.ok64 { None } else { Some(1 << 32) }
    }

    /// Zeroed DMA memory, below 4 GiB unless the controller reports 64-bit addressing.
    fn dma(&self, frames: u64) -> Option<u64> {
        memory::allocate_contiguous(frames, self.dma_limit())
    }

    // ---- the command transport ----

    /// Sends one verb through the CORB and waits for its response in the RIRB; `None` if none came within 5 ms
    /// (an absent codec or widget).
    ///
    /// Both pointers are "last used entry" registers: the new verb goes in slot `CORBWP + 1`, then `CORBWP` is
    /// updated; the controller writes responses at `RIRBWP + 1` and software tracks how far it has read, since
    /// there is no RIRB read pointer. Unsolicited responses (bit 4 of the entry's upper word) are skipped.
    fn command(&mut self, word: u32) -> Option<u32> {
        let regs = self.regs;
        let write_pointer = (read16(regs, REG_CORBWP) + 1) % self.corb_entries;
        unsafe { ((self.corb + 4 * u64::from(write_pointer)) as *mut u32).write_volatile(word) };
        write16(regs, REG_CORBWP, write_pointer);
        let mut answer = None;
        let entries = self.rirb_entries;
        wait_until(5000, || {
            while self.rirb_rp != read16(regs, REG_RIRBWP) % entries {
                self.rirb_rp = (self.rirb_rp + 1) % entries;
                let entry = (self.rirb + 8 * u64::from(self.rirb_rp)) as *const u32;
                let (response, extended) = unsafe { (entry.read_volatile(), entry.add(1).read_volatile()) };
                if extended & (1 << 4) == 0 {
                    answer = Some(response);
                    return true;
                }
            }
            false
        });
        answer
    }

    fn parameter(&mut self, cad: u8, nid: u8, parameter: u32) -> Option<u32> {
        self.command(verb(cad, nid, VERB_GET_PARAMETER, parameter))
    }

    /// Allocates and starts the CORB and RIRB. Sizes: the controller offers 2, 16 and/or 256 entries (bits 7:4 of
    /// the size register); the biggest is used.
    fn start_command_rings(&mut self) -> bool {
        let regs = self.regs;
        write8(regs, REG_CORBCTL, 0);
        write8(regs, REG_RIRBCTL, 0);
        let (Some(corb), Some(rirb)) = (self.dma(1), self.dma(1)) else { return false };
        self.corb = corb;
        self.rirb = rirb;
        let size_of = |register: u64| -> Option<(u8, u16)> {
            let capability = read8(regs, register) >> 4;
            let (select, entries) = if capability & 4 != 0 {
                (2, 256)
            } else if capability & 2 != 0 {
                (1, 16)
            } else if capability & 1 != 0 {
                (0, 2)
            } else {
                return None;
            };
            write8(regs, register, (read8(regs, register) & !3) | select);
            Some((select, entries))
        };
        let (Some((_, corb_entries)), Some((_, rirb_entries))) = (size_of(REG_CORBSIZE), size_of(REG_RIRBSIZE)) else { return false };
        self.corb_entries = corb_entries;
        self.rirb_entries = rirb_entries;
        write32(regs, REG_CORBLBASE, corb as u32);
        write32(regs, REG_CORBUBASE, if self.ok64 { (corb >> 32) as u32 } else { 0 });
        write32(regs, REG_RIRBLBASE, rirb as u32);
        write32(regs, REG_RIRBUBASE, if self.ok64 { (rirb >> 32) as u32 } else { 0 });
        // Reset the CORB read pointer: set the bit, wait for it to read back set, clear it, wait for it to read back
        // clear. Some chips never confirm one of the steps; carry on regardless (Linux does too), the pointers are
        // brought into agreement below.
        write16(regs, REG_CORBRP, CORBRP_RST);
        wait_until(1000, || read16(regs, REG_CORBRP) & CORBRP_RST != 0);
        write16(regs, REG_CORBRP, 0);
        wait_until(1000, || read16(regs, REG_CORBRP) & CORBRP_RST == 0);
        write16(regs, REG_CORBWP, 0);
        // The RIRB write pointer reset is a self-clearing pulse. Interrupt after every response would stall
        // nothing here, but is only wanted for the self-test: RINTCNT 255 means "never in practice".
        write16(regs, REG_RIRBWP, RIRBWP_RST);
        write16(regs, REG_RINTCNT, 0xff);
        self.rirb_rp = 0;
        write8(regs, REG_CORBCTL, CORBCTL_RUN);
        write8(regs, REG_RIRBCTL, RIRBCTL_DMA_EN);
        true
    }

    /// Resets the controller and waits for codecs to announce themselves. The spec wants the reset held at least
    /// 100 us ("for the codec PLL to settle"); real silicon that was entered and left back to back never completed
    /// the transition, which a 1 ms hold fixed (the first port of this to a 6-series PCH needed it).
    fn reset(&mut self) -> Option<u16> {
        let regs = self.regs;
        let control = read32(regs, REG_GCTL);
        write32(regs, REG_GCTL, control & !GCTL_CRST);
        if !wait_until(50_000, || read32(regs, REG_GCTL) & GCTL_CRST == 0) {
            return None;
        }
        apic::busy_wait_ms(1);
        write32(regs, REG_GCTL, (control | GCTL_CRST) & !(1 << 8));
        if !wait_until(50_000, || read32(regs, REG_GCTL) & GCTL_CRST != 0) {
            return None;
        }
        // Codecs need at least 521 us after the link comes up to signal presence; they usually take a few ms.
        // Wait for the first, then a little for the rest.
        wait_until(150_000, || read16(regs, REG_STATESTS) & 0x7fff != 0);
        apic::busy_wait_ms(2);
        let present = read16(regs, REG_STATESTS) & 0x7fff;
        write16(regs, REG_STATESTS, present); // write 1 to clear
        Some(present)
    }

    // ---- discovery ----

    fn enumerate_codec(&mut self, slot: usize, address: u8) {
        let Some(ids) = self.parameter(address, 0, PAR_VENDOR_ID) else { return };
        // Built in place: a `Codec` is several KiB, too much to copy around on the kernel stack for nothing.
        let codec = unsafe { &mut *(&mut self.codecs[slot] as *mut Codec) };
        codec.present = true;
        codec.address = address;
        codec.vendor = (ids >> 16) as u16;
        codec.device = ids as u16;
        // The root's subordinate nodes are function groups; the first of type 1 is the Audio Function Group.
        if let Some(nodes) = self.parameter(address, 0, PAR_NODE_COUNT) {
            let (first, count) = (((nodes >> 16) & 0xff) as u8, (nodes & 0xff) as u8);
            for offset in 0..count {
                let nid = first.wrapping_add(offset);
                if self.parameter(address, nid, PAR_FUNCTION_TYPE).is_some_and(|kind| kind & 0xff == 1) {
                    codec.afg = nid;
                    break;
                }
            }
        }
        if codec.afg != 0 {
            // Wake the function group before reading anything from it: a codec left in D3 answers parameter reads
            // but its converters do not run.
            self.command(verb(address, codec.afg, VERB_SET_POWER_STATE, 0));
            apic::busy_wait_ms(10);
            codec.afg_pcm = self.parameter(address, codec.afg, PAR_PCM).unwrap_or(0);
            codec.afg_amp_in = self.parameter(address, codec.afg, PAR_AMP_IN_CAP).unwrap_or(0);
            codec.afg_amp_out = self.parameter(address, codec.afg, PAR_AMP_OUT_CAP).unwrap_or(0);
            if let Some(nodes) = self.parameter(address, codec.afg, PAR_NODE_COUNT) {
                codec.first_nid = ((nodes >> 16) & 0xff) as u8;
                codec.total = (nodes & 0xff) as usize;
                for offset in 0..codec.total.min(MAX_WIDGETS) {
                    let nid = codec.first_nid.wrapping_add(offset as u8);
                    codec.widgets[offset] = self.read_widget(address, nid, (codec.afg_pcm, codec.afg_amp_in, codec.afg_amp_out));
                    codec.count = offset + 1;
                }
            }
        }
    }

    /// Reads one widget's description. `afg` is the function group's (PCM, input amp, output amp) capabilities,
    /// which a widget without its own override inherits.
    fn read_widget(&mut self, cad: u8, nid: u8, afg: (u32, u32, u32)) -> Widget {
        let mut widget = Widget::EMPTY;
        widget.nid = nid;
        widget.caps = self.parameter(cad, nid, PAR_WIDGET_CAP).unwrap_or(0);
        widget.kind = ((widget.caps >> 20) & 0xf) as u8;
        if widget.kind == KIND_PIN {
            widget.pin_caps = self.parameter(cad, nid, PAR_PIN_CAP).unwrap_or(0);
            widget.config = self.command(verb(cad, nid, VERB_GET_CONFIG_DEFAULT, 0)).unwrap_or(0);
        }
        if widget.kind == KIND_OUTPUT || widget.kind == KIND_INPUT {
            // A converter without its own format override uses the function group's.
            widget.pcm = if widget.caps & WCAP_FORMAT_OVERRIDE != 0 { self.parameter(cad, nid, PAR_PCM).unwrap_or(0) } else { afg.0 };
            if widget.caps & WCAP_FORMAT_OVERRIDE != 0 && self.parameter(cad, nid, PAR_STREAMS).unwrap_or(0) & 1 == 0 {
                widget.pcm = 0; // does not take PCM
            }
        }
        widget.amp_in = if widget.caps & WCAP_AMP_OVERRIDE != 0 { self.parameter(cad, nid, PAR_AMP_IN_CAP).unwrap_or(0) } else { afg.1 };
        widget.amp_out = if widget.caps & WCAP_AMP_OVERRIDE != 0 { self.parameter(cad, nid, PAR_AMP_OUT_CAP).unwrap_or(0) } else { afg.2 };
        if widget.caps & WCAP_CONN_LIST != 0 {
            self.read_connections(cad, &mut widget);
        }
        widget
    }

    /// Reads a connection list. Entries are 8 bits (four per response) or 16 bits (two per response) as bit 7 of the
    /// length says; an entry with its top bit set means "every nid from the previous entry + 1 up to this one".
    fn read_connections(&mut self, cad: u8, widget: &mut Widget) {
        let Some(length_word) = self.parameter(cad, widget.nid, PAR_CONN_LEN) else { return };
        let long = length_word & 0x80 != 0;
        let length = (length_word & 0x7f) as usize;
        let (per_reply, bits) = if long { (2, 16) } else { (4, 8) };
        let mut previous: Option<u16> = None;
        let mut index = 0;
        while index < length && usize::from(widget.conn_count) < MAX_CONNS {
            let Some(reply) = self.command(verb(cad, widget.nid, VERB_GET_CONNECT_LIST, index as u32)) else { return };
            for part in 0..per_reply {
                if index + part >= length {
                    break;
                }
                let entry = ((reply >> (part * bits)) & ((1 << bits) - 1)) as u16;
                let (is_range, nid) = if long { (entry & 0x8000 != 0, entry & 0x7fff) } else { (entry & 0x80 != 0, entry & 0x7f) };
                if is_range {
                    if let Some(from) = previous {
                        for between in from + 1..=nid {
                            if usize::from(widget.conn_count) < MAX_CONNS {
                                widget.conns[usize::from(widget.conn_count)] = between as u8;
                                widget.conn_count += 1;
                            }
                        }
                    }
                } else if usize::from(widget.conn_count) < MAX_CONNS {
                    widget.conns[usize::from(widget.conn_count)] = nid as u8;
                    widget.conn_count += 1;
                }
                previous = Some(nid);
            }
            index += per_reply;
        }
    }

    // ---- path search ----

    /// Scores a pin by what its default configuration says it is, or `None` if nothing is wired to it. Jack pins
    /// that can sense a plug get a bonus for having one plugged in and a penalty for not (a laptop's unplugged
    /// headphone jack is a worse choice than its speaker). A pin whose codec does not say (configuration 0)
    /// scores in the middle.
    fn pin_score(&mut self, cad: u8, pin: &Widget, capture: bool) -> Option<u8> {
        let config = pin.config;
        let connectivity = config >> 30;
        if config != 0 && connectivity == 1 {
            return None; // no physical connection
        }
        let device = (config >> 20) & 0xf;
        let base: i32 = if config == 0 {
            2
        } else if capture {
            match device {
                DEV_LINE_IN => 5,
                DEV_MIC_IN => 4,
                DEV_AUX => 3,
                DEV_CD => 2,
                _ => 1,
            }
        } else {
            match device {
                DEV_LINE_OUT => 5,
                DEV_HP_OUT => 4,
                DEV_SPEAKER => 3,
                _ => 1,
            }
        };
        let mut score = base;
        if pin.pin_caps & PINCAP_PRESENCE != 0 && connectivity == 0 {
            let plugged = self.command(verb(cad, pin.nid, VERB_GET_PIN_SENSE, 0)).is_some_and(|sense| sense & (1 << 31) != 0);
            score += if plugged { 10 } else { -3 };
        }
        Some(score.clamp(0, 30) as u8)
    }

    /// Finds the best path for one direction: playback paths start at output pins and descend to a DAC; capture
    /// paths start at an ADC and descend to an input pin.
    fn find_path(&mut self, capture: bool) -> Path {
        let mut best = Path::NONE;
        for slot in 0..MAX_CODECS {
            // Nothing changes `codecs` during the search; the reference only dodges borrowing `self` for the commands.
            let codec = unsafe { &*(&self.codecs[slot] as *const Codec) };
            if !codec.present {
                continue;
            }
            for start in 0..codec.count {
                let widget = codec.widgets[start];
                let is_start = if capture { widget.kind == KIND_INPUT && widget.pcm != 0 } else { widget.kind == KIND_PIN && widget.pin_caps & PINCAP_OUTPUT != 0 };
                if !is_start {
                    continue;
                }
                let mut nodes = [0u8; MAX_PATH];
                let mut selects = [0u8; MAX_PATH];
                nodes[0] = widget.nid;
                let mut budget = 4000u32;
                let mut found: [Path; 1] = [Path::NONE];
                let mut results = SearchResult { best: &mut found[0], codec: slot as u8 };
                self.descend(codec, capture, 1, &mut nodes, &mut selects, &mut budget, &mut results);
                if found[0].valid && (!best.valid || found[0].score > best.score) {
                    best = found[0];
                }
            }
        }
        best
    }

    /// Depth-first walk along connection lists from `nodes[depth - 1]`. For playback it ends at a DAC, for capture
    /// at an input-capable pin; every complete path is scored and the best kept in `result`.
    fn descend(&mut self, codec: &Codec, capture: bool, depth: usize, nodes: &mut [u8; MAX_PATH], selects: &mut [u8; MAX_PATH], budget: &mut u32, result: &mut SearchResult) {
        let Some(&here) = codec.widget(nodes[depth - 1]) else { return };
        for (index, &next) in here.conns[..usize::from(here.conn_count)].iter().enumerate() {
            if *budget == 0 {
                return;
            }
            *budget -= 1;
            let Some(&target) = codec.widget(next) else { continue };
            if nodes[..depth].contains(&next) {
                continue;
            }
            selects[depth - 1] = index as u8;
            nodes[depth] = next;
            let reached = if capture { target.kind == KIND_PIN && target.pin_caps & PINCAP_INPUT != 0 } else { target.kind == KIND_OUTPUT && target.pcm != 0 };
            if reached {
                let pin = if capture { target } else { *codec.widget(nodes[0]).unwrap() };
                if let Some(score) = self.pin_score(codec.address, &pin, capture) {
                    if !result.best.valid || score > result.best.score {
                        *result.best = Path { valid: true, codec: result.codec, len: (depth + 1) as u8, score, nodes: *nodes, selects: *selects };
                    }
                }
            } else if depth + 1 < MAX_PATH && matches!(target.kind, KIND_MIXER | KIND_SELECTOR) {
                self.descend(codec, capture, depth + 1, nodes, selects, budget, result);
            }
        }
    }

    // ---- bringing a path to life ----

    fn amp_gain(caps: u32) -> u32 {
        // 0 dB is the "offset" step; never ask for more steps than the amplifier has.
        let steps = (caps >> 8) & 0x7f;
        (caps & 0x7f).min(steps)
    }

    fn unmute_output(&mut self, cad: u8, widget: &Widget) {
        if widget.caps & WCAP_OUT_AMP != 0 {
            self.command(verb16(cad, widget.nid, VERB_SET_AMP_GAIN_MUTE, AMP_OUTPUT | AMP_LEFT | AMP_RIGHT | Self::amp_gain(widget.amp_out)));
        }
    }

    fn unmute_input(&mut self, cad: u8, widget: &Widget, index: u8) {
        if widget.caps & WCAP_IN_AMP != 0 {
            self.command(verb16(cad, widget.nid, VERB_SET_AMP_GAIN_MUTE, AMP_INPUT | AMP_LEFT | AMP_RIGHT | (u32::from(index) << 8) | Self::amp_gain(widget.amp_in)));
        }
    }

    /// Programs every widget on a path: power, connection selects, unmuted amplifiers, and the pin.
    fn configure_path(&mut self, path: &Path, capture: bool) {
        let codec = unsafe { &*(&self.codecs[usize::from(path.codec)] as *const Codec) };
        let cad = codec.address;
        for position in 0..usize::from(path.len) {
            let Some(&widget) = codec.widget(path.nodes[position]) else { continue };
            if widget.caps & WCAP_POWER != 0 {
                self.command(verb(cad, widget.nid, VERB_SET_POWER_STATE, 0));
            }
            if position + 1 < usize::from(path.len) {
                let select = path.selects[position];
                if widget.conn_count > 1 && widget.kind != KIND_MIXER {
                    self.command(verb(cad, widget.nid, VERB_SET_CONNECT_SEL, u32::from(select)));
                }
                self.unmute_input(cad, &widget, select);
            }
            self.unmute_output(cad, &widget);
        }
        let Some(&pin) = codec.widget(path.pin(capture)) else { return };
        let device = (pin.config >> 20) & 0xf;
        if capture {
            // A microphone pin wants a bias voltage (VREF); 80% is the usual electret choice.
            let vref = if device == DEV_MIC_IN && pin.pin_caps & PINCAP_VREF80 != 0 { PINCTL_VREF80 } else { 0 };
            self.command(verb(cad, pin.nid, VERB_SET_PIN_WIDGET_CONTROL, PINCTL_IN_EN | vref));
        } else {
            let control = PINCTL_OUT_EN | if device == DEV_HP_OUT { PINCTL_HP_EN } else { 0 };
            self.command(verb(cad, pin.nid, VERB_SET_PIN_WIDGET_CONTROL, control));
            if pin.pin_caps & PINCAP_EAPD != 0 {
                self.command(verb(cad, pin.nid, VERB_SET_EAPD, 2)); // the external amplifier (laptop speakers) enable
            }
        }
    }

    /// The converter widget of a direction's path, and its codec's address.
    fn converter(&self, capture: bool) -> Option<(u8, Widget)> {
        let path = if capture { &self.capture_path } else { &self.playback_path };
        if !path.valid {
            return None;
        }
        let codec = &self.codecs[usize::from(path.codec)];
        let widget = *codec.widget(path.converter(capture))?;
        Some((codec.address, widget))
    }

    /// The sample sizes a converter offers, best first: (bits, container bytes).
    fn sizes(widget: &Widget) -> ([(u8, u8); 2], usize) {
        let mut sizes = [(0u8, 0u8); 2];
        let mut count = 0;
        if widget.pcm & (1 << 19) != 0 {
            sizes[count] = (24, 4);
            count += 1;
        }
        if widget.pcm & (1 << 17) != 0 || count == 0 {
            sizes[count] = (16, 2); // the baseline, also when a converter's report is unreadable
            count += 1;
        }
        (sizes, count)
    }

    /// The modes of a direction: (channels, bits), best first. The best is the most channels, then the most bits.
    fn modes(&self, capture: bool) -> ([(u8, u8); 4], usize) {
        let mut modes = [(0u8, 0u8); 4];
        let mut count = 0;
        let Some((_, widget)) = self.converter(capture) else { return (modes, 0) };
        let maximum = widget.max_channels().min(8);
        let (sizes, size_count) = Self::sizes(&widget);
        for &channels in &[maximum, 2.min(maximum)] {
            for &(bits, _) in &sizes[..size_count] {
                if count < modes.len() && !modes[..count].contains(&(channels, bits)) {
                    modes[count] = (channels, bits);
                    count += 1;
                }
            }
        }
        (modes, count)
    }

    fn mode(&self, capture: bool) -> Option<(u8, u8)> {
        let (modes, count) = self.modes(capture);
        let picked = self.picked[usize::from(!capture)];
        if usize::from(picked) < count {
            Some(modes[usize::from(picked)])
        } else if count > 0 {
            Some(modes[0])
        } else {
            None
        }
    }

    /// The sample rates every converter on the active paths offers, in `RATE_ORDER`.
    fn rates(&self) -> ([u32; 8], usize) {
        let mut mask = u32::MAX;
        for capture in [false, true] {
            if let Some((_, widget)) = self.converter(capture) {
                mask &= widget.pcm & 0xfff;
            }
        }
        let mut rates = [0u32; 8];
        let mut count = 0;
        for &rate in &RATE_ORDER {
            let bit = RATES.iter().position(|entry| entry.0 == rate).unwrap();
            if mask & (1 << bit) != 0 && count < rates.len() {
                rates[count] = rate;
                count += 1;
            }
        }
        (rates, count)
    }

    fn usable(&self) -> bool {
        self.present && (self.playback_path.valid || self.capture_path.valid)
    }

    fn stream(&mut self, capture: bool) -> &mut Stream {
        if capture { &mut self.capture } else { &mut self.playback }
    }

    // ---- streams ----

    fn stream_registers(&self, index: u8) -> u64 {
        self.regs + SD_BASE + SD_STRIDE * u64::from(index)
    }

    /// Stops a stream descriptor: clear RUN, wait for the controller to stop, then put the descriptor through its
    /// reset handshake (set `SRST`, see it read back set, clear it, see it read back clear), which also clears its
    /// pointers.
    fn reset_stream(&self, index: u8) -> bool {
        let registers = self.stream_registers(index);
        write32(registers, SD_CTL, read32(registers, SD_CTL) & 0x00ff_ffff & !(SD_CTL_RUN | SD_CTL_IOCE));
        wait_until(2000, || read32(registers, SD_CTL) & SD_CTL_RUN == 0);
        write32(registers, SD_CTL, SD_CTL_SRST);
        let entered = wait_until(2000, || read32(registers, SD_CTL) & SD_CTL_SRST != 0);
        write32(registers, SD_CTL, 0);
        let left = wait_until(2000, || read32(registers, SD_CTL) & SD_CTL_SRST == 0);
        write8(registers, SD_STS, SD_STS_BCIS | SD_STS_ERRORS);
        entered && left
    }

    fn update_interrupt_control(&self) {
        let mut mask = 0u32;
        if self.delivery != Delivery::Polled {
            for stream in [&self.playback, &self.capture] {
                if stream.active.load(Ordering::Acquire) {
                    mask |= 1 << stream.index;
                }
            }
        }
        write32(self.regs, REG_INTCTL, if mask != 0 { INTCTL_GIE | INTCTL_CIE | mask } else { 0 });
    }

    fn start(&mut self, rate: u32, want_capture: bool, want_playback: bool) -> Result<(), &'static str> {
        if self.playback.active.load(Ordering::Acquire) || self.capture.active.load(Ordering::Acquire) {
            return Err("already running");
        }
        let Some(rate_bit) = RATES.iter().position(|entry| entry.0 == rate) else { return Err("the device does not offer that sample rate") };
        // Check everything before touching the hardware.
        for (wanted, capture) in [(want_capture, true), (want_playback, false)] {
            if !wanted {
                continue;
            }
            let Some((_, widget)) = self.converter(capture) else {
                return Err(if capture { "the device has no capture stream" } else { "the device has no playback stream" });
            };
            if widget.pcm & (1 << rate_bit) == 0 {
                return Err("the device does not offer that sample rate");
            }
        }
        // The first start after a quiet spell can leave the DMA engine stalled on real hardware (an Intel 6 series PCH,
        // 8086:1c20: RUN set, LPIB stays 0, no buffer ever completes, so playback is never drained); stopping the streams
        // and starting them again works. Up to three attempts; the last one is kept whatever it does.
        for attempt in 1..=3 {
            if want_capture {
                self.build_stream(true, rate_bit)?;
            }
            if want_playback {
                if let Err(reason) = self.build_stream(false, rate_bit) {
                    self.stop();
                    return Err(reason);
                }
            }
            if attempt == 3 || self.streams_moving() {
                break;
            }
            self.stop();
        }
        Ok(())
    }

    /// True once every running stream's link position has left 0 (within 20 ms: a period is 10 ms).
    fn streams_moving(&self) -> bool {
        [&self.playback, &self.capture].iter().filter(|stream| stream.active.load(Ordering::Acquire)).all(|stream| {
            let registers = self.stream_registers(stream.index);
            wait_until(20_000, || read32(registers, SD_LPIB) != 0)
        })
    }

    fn build_stream(&mut self, capture: bool, rate_bit: usize) -> Result<(), &'static str> {
        let path = if capture { self.capture_path } else { self.playback_path };
        let (cad, converter) = self.converter(capture).ok_or("no path")?;
        let (channels, bits) = self.mode(capture).ok_or("no mode")?;
        let container = if bits == 16 { 2 } else { 4 };
        let (_, base44, multiplier, divisor) = RATES[rate_bit];
        let format = (u32::from(base44) << 14) | ((multiplier - 1) << 11) | ((divisor - 1) << 8) | (if bits == 16 { 0b001 } else { 0b011 } << 4) | (u32::from(channels) - 1);
        let index = if capture { 0 } else { self.input_streams };
        if (capture && self.input_streams == 0) || (!capture && self.output_streams == 0) {
            return Err(if capture { "the controller has no input stream" } else { "the controller has no output stream" });
        }
        let tag = if capture { CAPTURE_TAG } else { PLAYBACK_TAG };
        let frame_bytes = usize::from(channels) * container;
        let period_bytes = PERIOD_FRAMES * frame_bytes;
        let buffer_bytes = period_bytes * PERIODS;
        let buffer_frames = buffer_bytes.div_ceil(4096) as u64;
        let buffer = self.dma(buffer_frames).ok_or("no memory for the audio buffer")?;
        let Some(bdl) = self.dma(1) else {
            memory::free(buffer, buffer_frames);
            return Err("no memory for the buffer descriptor list");
        };
        let ring = if capture { crate::usb_audio::allocate_capture_ring() } else { PcmRing::allocate(crate::usb_audio::RING_BYTES) };
        let Some(ring) = ring else {
            memory::free(buffer, buffer_frames);
            memory::free(bdl, 1);
            return Err("no memory for the PCM ring");
        };
        if capture {
            // Not left as the zeros the allocator gives: a controller that never writes the buffer (a broken DMA
            // path, a stream tag that matches nothing) would otherwise look exactly like a quiet input. A recognisable
            // pattern shows up as loud noise in a level meter instead; a working one overwrites it before it is read.
            unsafe { core::ptr::write_bytes(buffer as *mut u8, 0x55, buffer_bytes) };
        }
        // The buffer descriptor list: one 16-byte entry per period: 64-bit address, length, and bit 0 = interrupt on
        // completion.
        for period in 0..PERIODS {
            let entry = (bdl + 16 * period as u64) as *mut u64;
            unsafe {
                entry.write_volatile(buffer + (period * period_bytes) as u64);
                (entry.add(1) as *mut u32).write_volatile(period_bytes as u32);
                (entry.add(1) as *mut u32).add(1).write_volatile(1);
            }
        }
        if !self.reset_stream(index) {
            memory::free(buffer, buffer_frames);
            memory::free(bdl, 1);
            ring.free();
            return Err("the stream descriptor would not reset");
        }
        let registers = self.stream_registers(index);
        write32(registers, SD_CBL, buffer_bytes as u32);
        write16(registers, SD_LVI, (PERIODS - 1) as u16);
        write16(registers, SD_FMT, format as u16);
        write32(registers, SD_BDPL, bdl as u32);
        write32(registers, SD_BDPU, if self.ok64 { (bdl >> 32) as u32 } else { 0 });
        // Tell the converter its format and which stream (tag) to listen to or talk on, then open the path.
        self.command(verb16(cad, converter.nid, VERB_SET_STREAM_FORMAT, format));
        self.command(verb(cad, converter.nid, VERB_SET_CHANNEL_STREAMID, u32::from(tag) << 4));
        if channels > 2 {
            self.command(verb(cad, converter.nid, VERB_SET_CVT_CHAN_COUNT, u32::from(channels) - 1));
        }
        self.configure_path(&path, capture);

        let was = cpu::interrupts_save_and_disable();
        let stream = self.stream(capture);
        stream.index = index;
        stream.tag = tag as u8;
        stream.capture = capture;
        stream.channels = channels;
        stream.bits = bits;
        stream.container = container as u8;
        stream.rate = RATES[rate_bit].0;
        stream.format = format as u16;
        stream.buffer = buffer;
        stream.buffer_frames = buffer_frames;
        stream.bdl = bdl;
        stream.period_bytes = period_bytes;
        stream.next = 0;
        stream.pcm = ring;
        stream.periods.store(0, Ordering::Relaxed);
        stream.bytes.store(0, Ordering::Relaxed);
        stream.errors.store(0, Ordering::Relaxed);
        stream.underruns.store(0, Ordering::Relaxed);
        stream.primed.store(false, Ordering::Relaxed);
        stream.path = path;
        stream.active.store(true, Ordering::Release);
        self.update_interrupt_control();
        write32(registers, SD_CTL, (u32::from(tag) << SD_CTL_TAG_SHIFT) | SD_CTL_IOCE | SD_CTL_RUN);
        cpu::interrupts_restore(was);
        Ok(())
    }

    fn stop(&mut self) {
        for capture in [false, true] {
            if !self.stream(capture).active.load(Ordering::Acquire) {
                continue;
            }
            // Unpublish first (interrupts off) so the handler stops touching the stream, then stop the hardware.
            let was = cpu::interrupts_save_and_disable();
            self.stream(capture).active.store(false, Ordering::Release);
            self.update_interrupt_control();
            cpu::interrupts_restore(was);
            let stream = self.stream(capture);
            let (index, buffer, frames, bdl) = (stream.index, stream.buffer, stream.buffer_frames, stream.bdl);
            self.reset_stream(index);
            if let Some((cad, converter)) = self.converter(capture) {
                self.command(verb(cad, converter.nid, VERB_SET_CHANNEL_STREAMID, 0)); // back to the unused stream 0
            }
            let stream = self.stream(capture);
            stream.pcm.free();
            stream.pcm = PcmRing::empty();
            memory::free(buffer, frames);
            memory::free(bdl, 1);
        }
    }

    /// Where the controller is in a stream's buffer, in bytes: from the DMA position buffer in host memory when it
    /// works, else the `LPIB` register. (A position buffer entry that reads 0 while `LPIB` does not is a controller
    /// that ignores the buffer: believe the register.)
    fn position(&self, index: u8) -> usize {
        let lpib = read32(self.stream_registers(index), SD_LPIB);
        if self.position_buffer_works {
            let entry = (self.position_buffer + 8 * u64::from(index)) as *const u32;
            let value = unsafe { entry.read_volatile() };
            if value != 0 || lpib == 0 {
                return value as usize;
            }
        }
        lpib as usize
    }

    /// Services both streams: every period the hardware has finished since last time. Runs in the interrupt
    /// handler, or with interrupts off from `poll`.
    fn service(&mut self) {
        for capture in [false, true] {
            let position = {
                let stream = if capture { &self.capture } else { &self.playback };
                if !stream.active.load(Ordering::Acquire) {
                    continue;
                }
                let registers = self.stream_registers(stream.index);
                let status = read8(registers, SD_STS);
                if status != 0 {
                    write8(registers, SD_STS, status & (SD_STS_BCIS | SD_STS_ERRORS));
                    if status & SD_STS_ERRORS != 0 {
                        stream.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
                self.position(stream.index)
            };
            let stream = if capture { &mut self.capture } else { &mut self.playback };
            let current = (position / stream.period_bytes).min(PERIODS - 1);
            // Every period from `next` up to (not including) the one the hardware is in has been completed.
            let mut guard = 0;
            while stream.next != current && guard < PERIODS {
                let period = (stream.buffer + (stream.next * stream.period_bytes) as u64) as *mut u8;
                let area = unsafe { core::slice::from_raw_parts_mut(period, stream.period_bytes) };
                if capture {
                    // The whole period or none of it, so a full ring never leaves the channels shifted.
                    stream.pcm.push_whole(area);
                } else {
                    let frame = stream.frame_bytes();
                    let wanted = (stream.pcm.available().min(stream.period_bytes) / frame) * frame;
                    let got = stream.pcm.pop(&mut area[..wanted]);
                    if got != 0 {
                        stream.primed.store(true, Ordering::Relaxed);
                    }
                    if got < stream.period_bytes {
                        area[got..].fill(0);
                        if stream.primed.load(Ordering::Relaxed) {
                            stream.underruns.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
                stream.periods.fetch_add(1, Ordering::Relaxed);
                stream.bytes.fetch_add(stream.period_bytes as u64, Ordering::Relaxed);
                stream.next = (stream.next + 1) % PERIODS;
                guard += 1;
            }
        }
    }

    fn interrupt(&mut self) {
        let status = read32(self.regs, REG_INTSTS);
        if status == 0 {
            return; // a shared line, and not ours
        }
        self.interrupts.fetch_add(1, Ordering::Relaxed);
        if status & INTSTS_CIS != 0 {
            let response = read8(self.regs, REG_RIRBSTS);
            if response & RIRBSTS_CLEAR != 0 {
                write8(self.regs, REG_RIRBSTS, response & RIRBSTS_CLEAR);
            }
        }
        self.service();
    }

    /// Proves an interrupt path works without any audio running: enable the RIRB response interrupt, send one
    /// verb, and see whether the interrupt arrives (`arrived` reads the delivery mechanism's own counter).
    fn self_test(&mut self, arrived: impl Fn() -> u64) -> bool {
        let regs = self.regs;
        write8(regs, REG_RIRBSTS, RIRBSTS_CLEAR);
        write16(regs, REG_RINTCNT, 1);
        write8(regs, REG_RIRBCTL, RIRBCTL_DMA_EN | RIRBCTL_INT_EN);
        write32(regs, REG_INTCTL, INTCTL_GIE | INTCTL_CIE);
        let before = arrived();
        let address = self.codecs.iter().find(|codec| codec.present).map_or(0, |codec| codec.address);
        self.command(verb(address, 0, VERB_GET_PARAMETER, PAR_VENDOR_ID));
        let start = apic::ticks();
        while arrived() == before && apic::ticks().wrapping_sub(start) < 3 {
            core::hint::spin_loop();
        }
        let ok = arrived() != before;
        write32(regs, REG_INTCTL, 0);
        write8(regs, REG_RIRBCTL, RIRBCTL_DMA_EN);
        write16(regs, REG_RINTCNT, 0xff);
        write8(regs, REG_RIRBSTS, RIRBSTS_CLEAR);
        ok
    }

    fn enable_interrupts(&mut self, index: usize, no_msi: bool) -> Result<Delivery, &'static str> {
        if no_msi {
            return Err("settings: no-msi");
        }
        let device = self.pci;
        let mut reason = "no interrupt path passed its self-test";
        if device.find_capability(0x05).is_some() {
            match msi::allocate(isr, index) {
                Some(vector) if device.enable_msi(vector, apic::id()) => {
                    if self.self_test(|| msi::count(vector)) {
                        return Ok(Delivery::Msi(vector));
                    }
                    device.disable_msi();
                    msi::release(vector);
                    reason = "the MSI self-test message never arrived";
                }
                Some(vector) => msi::release(vector),
                None => reason = "no free MSI vector",
            }
        } else {
            reason = "no MSI capability";
        }
        // Legacy INTx: the firmware's line (a hint on APIC systems) and the pins an Intel or QEMU chipset
        // routes to the I/O APIC (GSI 16-23), probed because the ACPI routing table is not read.
        let (first, end) = crate::ioapic::gsi_range();
        let mut candidates = [u32::MAX; 9];
        candidates[0] = u32::from(device.interrupt_line());
        for (slot, gsi) in (16..24u32).enumerate() {
            candidates[slot + 1] = gsi;
        }
        for (position, &gsi) in candidates.iter().enumerate() {
            if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                continue;
            }
            let Some(vector) = msi::attach_intx(gsi, isr, index) else { continue };
            if self.self_test(|| msi::intx_count(vector)) {
                return Ok(Delivery::Intx { gsi, vector });
            }
            msi::detach_intx(vector, index);
            reason = "no I/O APIC pin delivered the self-test interrupt";
        }
        Err(reason)
    }
}

struct SearchResult<'a> {
    best: &'a mut Path,
    codec: u8,
}

/// The MSI/INTx handler: the controller's index is the context.
fn isr(context: usize) {
    let hda = controller(context);
    if hda.present {
        hda.interrupt();
    }
}

// ---- chipset quirks (found by Linux's hda_intel.c; unverified here) ----

/// Some chipsets route HDA DMA through a traffic class or a snoop setting that must be set for the audio buffers
/// to be coherent with the CPU's caches.
fn chipset_quirks(device: &pci::Device) {
    match device.vendor {
        // Intel: clear the traffic-class select (TCSEL, config 0x44 bits 2:0) so DMA uses traffic class 0.
        0x8086 => {
            let value = device.read32(0x44);
            device.write32(0x44, value & !0x7);
        }
        // AMD/ATI SB4x0-SB7x0: bit 1 of config 0x42 enables snooping of the controller's DMA.
        0x1002 if matches!(device.id, 0x437b | 0x4383) => {
            let value = device.read32(0x40);
            device.write32(0x40, (value & !(0x07 << 16)) | (0x02 << 16));
        }
        // NVIDIA nForce/MCP (device IDs below 0x0e00; their GPUs' HDMI audio has higher IDs): enable the
        // coherency bits in config 0x4c, 0x4d and 0x4e.
        0x10de if device.id < 0x0e00 => {
            let low = device.read32(0x4c);
            device.write32(0x4c, low | (1 << 0) | (1 << 8));
            let high = device.read32(0x4c);
            device.write32(0x4c, high | (0x0f << 16));
        }
        _ => {}
    }
}

// ---- bring-up ----

fn init_controller(index: usize, device: &pci::Device, no_msi: bool) -> bool {
    let hda = controller(index);
    // The slot is zero from boot (it lives in `.bss`); only a failed earlier attempt can have left codecs behind.
    for codec in hda.codecs.iter_mut() {
        codec.present = false;
        codec.count = 0;
    }
    hda.pci = *device;
    device.force_power_state_d0();
    device.enable_memory_and_bus_master();
    let Some((base, size)) = device.memory_bar(0) else { return false };
    paging::map_device(base, size.clamp(0x1000, MMIO_BYTES));
    hda.regs = base;
    chipset_quirks(device);
    let Some(present) = hda.reset() else {
        crate::println!("HD Audio {:02x}:{:02x}.{}: the controller would not leave reset.", device.bus, device.device, device.function);
        return false;
    };
    let capabilities = read16(base, REG_GCAP);
    hda.ok64 = capabilities & 1 != 0;
    hda.output_streams = (capabilities >> 12) as u8 & 0xf;
    hda.input_streams = (capabilities >> 8) as u8 & 0xf;
    hda.version = (read8(base, REG_VMIN + 1), read8(base, REG_VMIN));
    if !hda.start_command_rings() {
        crate::println!("HD Audio {:02x}:{:02x}.{}: could not set up the command rings.", device.bus, device.device, device.function);
        return false;
    }
    // The DMA position buffer: 8 bytes per stream descriptor, 128-byte aligned (a page is). Bit 0 enables it; a
    // controller that does not keep the bit does not have the feature.
    if let Some(buffer) = hda.dma(1) {
        hda.position_buffer = buffer;
        write32(base, REG_DPUBASE, if hda.ok64 { (buffer >> 32) as u32 } else { 0 });
        write32(base, REG_DPLBASE, buffer as u32 | 1);
        hda.position_buffer_works = read32(base, REG_DPLBASE) & 1 != 0;
    }
    hda.present = true; // the interrupt handler may now look at it
    let mut slot = 0;
    for address in 0..15u8 {
        if present & (1 << address) != 0 && slot < MAX_CODECS {
            hda.enumerate_codec(slot, address);
            if hda.codecs[slot].present {
                slot += 1;
            }
        }
    }
    hda.playback_path = hda.find_path(false);
    hda.capture_path = hda.find_path(true);
    match hda.enable_interrupts(index, no_msi) {
        Ok(delivery) => hda.delivery = delivery,
        Err(reason) => {
            hda.delivery = Delivery::Polled;
            crate::println!("  HD Audio interrupts: polled ({reason}).");
        }
    }
    true
}

/// Finds and brings up every HD Audio controller (PCI class 04, subclass 03). Needs interrupts enabled.
pub fn init_all(no_msi: bool) {
    let mut found = 0;
    for device in pci::devices().filter(|device| device.class == 0x04 && device.subclass == 0x03) {
        if found >= MAX_CONTROLLERS {
            break;
        }
        if !init_controller(found, &device, no_msi) {
            controller(found).present = false;
            continue;
        }
        let hda = controller(found);
        unsafe { COUNT = found + 1 };
        found += 1;
        crate::println!(
            "HD Audio {:02x}:{:02x}.{} ({:04x}:{:04x}): version {}.{}, {} input / {} output stream(s), {}-bit DMA, {} codec(s).",
            device.bus,
            device.device,
            device.function,
            device.vendor,
            device.id,
            hda.version.0,
            hda.version.1,
            hda.input_streams,
            hda.output_streams,
            if hda.ok64 { 64 } else { 32 },
            hda.codecs.iter().filter(|codec| codec.present).count()
        );
        for codec in hda.codecs.iter().filter(|codec| codec.present) {
            crate::println!("  codec {} {:04x}:{:04x}: audio function group {:#x}, {} widget(s).", codec.address, codec.vendor, codec.device, codec.afg, codec.total);
        }
        let describe_path = |path: &Path, capture: bool, name: &str| {
            if path.valid {
                crate::println!("  {name}: pin {:#x} via {} widget(s) {} {:#x} (score {}).", path.pin(capture), path.len - 1, if capture { "from" } else { "to" }, path.converter(capture), path.score);
            } else {
                crate::println!("  {name}: none found.");
            }
        };
        describe_path(&hda.playback_path, false, "playback path");
        describe_path(&hda.capture_path, true, "capture path");
        match hda.delivery {
            Delivery::Msi(vector) => crate::println!("  interrupts: MSI on vector {vector:#x} (the self-test message arrived)."),
            Delivery::Intx { gsi, vector } => crate::println!("  interrupts: legacy INTx, I/O APIC pin {gsi} on vector {vector:#x} (found by probing; the self-test interrupt arrived)."),
            Delivery::Polled => {}
        }
    }
}

// ---- the audio facade's view: one controller at a time ----

pub fn present(index: usize) -> bool {
    index < count() && controller(index).usable()
}

/// A score for choosing among controllers when the user has not: a controller with both a playback and a capture
/// path beats one with a single path; ties go to the first.
pub fn rank(index: usize) -> u32 {
    if !present(index) {
        return 0;
    }
    let hda = controller(index);
    1 + u32::from(hda.playback_path.score) + u32::from(hda.capture_path.score) + 100 * u32::from(hda.playback_path.valid) + 100 * u32::from(hda.capture_path.valid)
}

pub fn label(index: usize) -> (u8, u8, u8, u16, u16) {
    let hda = controller(index);
    (hda.pci.bus, hda.pci.device, hda.pci.function, hda.pci.vendor, hda.pci.id)
}

pub fn start(index: usize, rate: u32, capture: bool, playback: bool) -> Result<(), &'static str> {
    if !present(index) {
        return Err("no HD Audio device");
    }
    controller(index).start(rate, capture, playback)
}

pub fn stop(index: usize) {
    if index < count() {
        controller(index).stop();
    }
}

pub fn read(index: usize, out: &mut [u8]) -> usize {
    let hda = controller(index);
    if !hda.capture.active.load(Ordering::Acquire) {
        return 0;
    }
    poll_one(hda);
    hda.capture.pcm.pop(out)
}

pub fn write(index: usize, data: &[u8]) -> usize {
    let hda = controller(index);
    if !hda.playback.active.load(Ordering::Acquire) {
        return 0;
    }
    poll_one(hda);
    hda.playback.pcm.push(data)
}

pub fn playback_space(index: usize) -> usize {
    let hda = controller(index);
    if hda.playback.active.load(Ordering::Acquire) { hda.playback.pcm.space() } else { 0 }
}

/// (channels, bytes per sample slot) of a running stream.
pub fn format(index: usize, capture: bool) -> Option<(u8, u8)> {
    let hda = controller(index);
    let stream = if capture { &hda.capture } else { &hda.playback };
    stream.active.load(Ordering::Acquire).then(|| (stream.channels, stream.container))
}

/// Services the streams of a controller whose interrupts do not work (polled mode). With interrupts delivered the
/// handler does this and nothing is needed.
fn poll_one(hda: &mut Hda) {
    if hda.delivery == Delivery::Polled {
        let was = cpu::interrupts_save_and_disable();
        hda.service();
        cpu::interrupts_restore(was);
    }
}

pub fn poll() {
    for index in 0..count() {
        let hda = controller(index);
        if hda.present {
            poll_one(hda);
        }
    }
}

pub fn select(index: usize, capture: bool, mode: u32) -> Result<(), &'static str> {
    if !present(index) {
        return Err("no HD Audio device");
    }
    let hda = controller(index);
    let (_, count) = hda.modes(capture);
    let value = if mode == 0 {
        255
    } else if (mode as usize) <= count {
        (mode - 1) as u8
    } else {
        return Err("no such mode (`audio modes` lists them)");
    };
    hda.picked[usize::from(!capture)] = value;
    Ok(())
}

pub fn print_modes(index: usize) {
    let hda = controller(index);
    for (capture, name) in [(false, "Playback"), (true, "Capture")] {
        crate::println!("{name} modes (`audio use {} N`, or `auto`):", if capture { "capture" } else { "playback" });
        let (modes, count) = hda.modes(capture);
        let picked = hda.picked[usize::from(!capture)];
        let (rates, rate_count) = hda.rates();
        for number in 0..count {
            crate::print(format_args!("  {}{:>2}: {} ch, {}-bit,", if usize::from(picked) == number { '*' } else { ' ' }, number + 1, modes[number].0, modes[number].1));
            for rate in &rates[..rate_count] {
                crate::print(format_args!(" {rate}"));
            }
            crate::println!(" Hz");
        }
        if count == 0 {
            crate::println!("  none");
        } else if usize::from(picked) >= count {
            crate::println!("  (automatic: the mode with the most channels and bits)");
        }
    }
}

pub fn status(index: usize) {
    let hda = controller(index);
    let device = hda.pci;
    crate::print(format_args!("HD Audio {:02x}:{:02x}.{} sample rates:", device.bus, device.device, device.function));
    let (rates, count) = hda.rates();
    for rate in &rates[..count] {
        crate::print(format_args!(" {rate}"));
    }
    crate::println!();
    for (name, capture) in [("capture", true), ("playback", false)] {
        let stream = if capture { &hda.capture } else { &hda.playback };
        if stream.active.load(Ordering::Acquire) {
            crate::println!(
                "  {name}: {} Hz, {} ch {}-bit, {} periods, {} bytes, {} errors, {} underruns, ring {} bytes ({} overruns)",
                stream.rate,
                stream.channels,
                stream.bits,
                stream.periods.load(Ordering::Relaxed),
                stream.bytes.load(Ordering::Relaxed),
                stream.errors.load(Ordering::Relaxed),
                stream.underruns.load(Ordering::Relaxed),
                stream.pcm.available(),
                stream.pcm.overruns.load(Ordering::Relaxed)
            );
            // The hardware's view, for a stream that makes no progress: control (RUN is bit 1), status, link position
            // (LPIB), the position buffer's entry, and the controller's interrupt control and status.
            let registers = hda.stream_registers(stream.index);
            let entry = if hda.position_buffer == 0 { 0 } else { unsafe { ((hda.position_buffer + 8 * u64::from(stream.index)) as *const u32).read_volatile() } };
            crate::println!(
                "    SD{} ctl {:#08x} sts {:#04x} lpib {} dpib {} | intctl {:#010x} intsts {:#010x}",
                stream.index,
                read32(registers, SD_CTL) & 0xff_ffff,
                read8(registers, SD_STS),
                read32(registers, SD_LPIB),
                entry,
                read32(hda.regs, REG_INTCTL),
                read32(hda.regs, REG_INTSTS)
            );
            crate::println!(
                "    gctl {:#x} wallclk {} pci cmd {:#x} cbl {} lvi {} fifos {} fmt {:#x} bdl {:#x}/{:#x} dpl {:#x}",
                read32(hda.regs, REG_GCTL),
                read32(hda.regs, 0x30),
                hda.pci.read32(4) & 0xffff,
                read32(registers, SD_CBL),
                read16(registers, SD_LVI),
                read16(registers, 0x10),
                read16(registers, SD_FMT),
                read32(registers, SD_BDPU),
                read32(registers, SD_BDPL),
                read32(hda.regs, REG_DPLBASE)
            );
        } else {
            let path = if capture { &hda.capture_path } else { &hda.playback_path };
            match (path.valid, hda.mode(capture)) {
                (true, Some((channels, bits))) => crate::println!("  {name}: stopped ({channels} ch, {bits}-bit)"),
                _ => crate::println!("  {name}: none"),
            }
        }
    }
    match hda.delivery {
        Delivery::Msi(vector) => crate::println!("  interrupts: MSI vector {vector:#x}, {} received", hda.interrupts.load(Ordering::Relaxed)),
        Delivery::Intx { gsi, vector } => crate::println!("  interrupts: INTx pin {gsi} vector {vector:#x}, {} received", hda.interrupts.load(Ordering::Relaxed)),
        Delivery::Polled => crate::println!("  interrupts: polled"),
    }
}

pub fn fill_info(index: usize, info: &mut fosfor_abi::tape::AudioInfo) -> bool {
    if !present(index) {
        return false;
    }
    let hda = controller(index);
    let mut out = fosfor_abi::tape::AudioInfo::default();
    out.class_version = 0; // not USB: there is no audio class
    if let Some((channels, bits)) = hda.mode(false) {
        out.playback_channels = u32::from(channels);
        out.playback_bits = u32::from(bits);
        out.playback_slot_bytes = if bits == 16 { 2 } else { 4 };
    }
    if let Some((channels, bits)) = hda.mode(true) {
        out.capture_channels = u32::from(channels);
        out.capture_bits = u32::from(bits);
        out.capture_slot_bytes = if bits == 16 { 2 } else { 4 };
    }
    let (rates, count) = hda.rates();
    out.rate_count = count as u32;
    out.rates[..count].copy_from_slice(&rates[..count]);
    if hda.capture.active.load(Ordering::Acquire) {
        out.capture_rate = hda.capture.rate;
        out.capture_available = hda.capture.pcm.available() as u32;
        out.capture_channels = u32::from(hda.capture.channels);
        out.capture_bits = u32::from(hda.capture.bits);
        out.capture_slot_bytes = u32::from(hda.capture.container);
    }
    if hda.playback.active.load(Ordering::Acquire) {
        out.playback_rate = hda.playback.rate;
        out.playback_space = hda.playback.pcm.space() as u32;
        out.playback_channels = u32::from(hda.playback.channels);
        out.playback_bits = u32::from(hda.playback.bits);
        out.playback_slot_bytes = u32::from(hda.playback.container);
    }
    *info = out;
    true
}

pub fn fill_stats(index: usize, stats: &mut fosfor_abi::tape::AudioStats) {
    let hda = controller(index);
    let mut out = fosfor_abi::tape::AudioStats::default();
    if hda.capture.active.load(Ordering::Acquire) {
        out.capture_packets = hda.capture.periods.load(Ordering::Relaxed);
        out.capture_bytes = hda.capture.bytes.load(Ordering::Relaxed);
        out.capture_errors = hda.capture.errors.load(Ordering::Relaxed);
        out.capture_overruns = hda.capture.pcm.overruns.load(Ordering::Relaxed);
    }
    if hda.playback.active.load(Ordering::Acquire) {
        out.playback_packets = hda.playback.periods.load(Ordering::Relaxed);
        out.playback_errors = hda.playback.errors.load(Ordering::Relaxed);
        out.playback_underruns = hda.playback.underruns.load(Ordering::Relaxed);
    }
    *stats = out;
}

pub fn buffer_info(index: usize) -> (u64, u64) {
    let hda = controller(index);
    if hda.capture.active.load(Ordering::Acquire) { (hda.capture.pcm.size as u64, hda.capture.pcm.available() as u64) } else { (0, 0) }
}

/// The `hda` command: every controller, its codecs, their widgets and the paths chosen, for working out what an
/// unknown machine's codec presents.
pub fn describe() {
    if count() == 0 {
        return crate::println!("No HD Audio controller.");
    }
    for index in 0..count() {
        let hda = controller(index);
        let device = hda.pci;
        crate::println!(
            "HD Audio {index}: {:02x}:{:02x}.{} {:04x}:{:04x}, registers {:#x}, {} input / {} output streams, {}-bit DMA, CORB {} / RIRB {} entries, position buffer {}",
            device.bus,
            device.device,
            device.function,
            device.vendor,
            device.id,
            hda.regs,
            hda.input_streams,
            hda.output_streams,
            if hda.ok64 { 64 } else { 32 },
            hda.corb_entries,
            hda.rirb_entries,
            if hda.position_buffer_works { "on" } else { "off (LPIB)" }
        );
        for codec in hda.codecs.iter().filter(|codec| codec.present) {
            crate::println!("  codec {} {:04x}:{:04x}, AFG {:#x}, {} widget(s) from {:#x}{}", codec.address, codec.vendor, codec.device, codec.afg, codec.total, codec.first_nid, if codec.total > codec.count { " (list truncated)" } else { "" });
            for widget in &codec.widgets[..codec.count] {
                let kind = ["output", "input", "mixer", "selector", "pin", "power", "volume", "beep"].get(usize::from(widget.kind)).copied().unwrap_or("other");
                crate::print(format_args!("    {:#04x} {:<8} caps {:08x}", widget.nid, kind, widget.caps));
                if widget.kind == KIND_PIN {
                    crate::print(format_args!(" pincap {:08x} config {:08x}", widget.pin_caps, widget.config));
                }
                if widget.kind == KIND_OUTPUT || widget.kind == KIND_INPUT {
                    crate::print(format_args!(" pcm {:08x}", widget.pcm));
                }
                if widget.caps & WCAP_DIGITAL != 0 {
                    crate::print(format_args!(" digital"));
                }
                if widget.conn_count > 0 {
                    crate::print(format_args!(" <-"));
                    for connection in &widget.conns[..usize::from(widget.conn_count)] {
                        crate::print(format_args!(" {connection:#x}"));
                    }
                }
                crate::println!();
            }
        }
        for (capture, name) in [(false, "playback"), (true, "capture")] {
            let path = if capture { &hda.capture_path } else { &hda.playback_path };
            if path.valid {
                crate::print(format_args!("  {name} path (score {}):", path.score));
                for node in &path.nodes[..usize::from(path.len)] {
                    crate::print(format_args!(" {node:#x}"));
                }
                crate::println!();
            } else {
                crate::println!("  {name} path: none");
            }
        }
    }
}
