//! Minimal driver for the Intel HD Audio ("HDA") controller -- CORB/RIRB
//! command transport, real node-graph/widget enumeration,
//! multi-controller discovery (up to `MAX_INSTANCES`), and real
//! stream-DMA playback (one DAC output stream per controller). See
//! AGENTS.md's "Intel HDA" sections for the fuller research/scoping
//! writeup this and its three prior steps follow.
//!
//! HDA is architecturally very different from `es1370.rs`. ES1370 is one
//! flat, self-contained register set: every DAC1/ADC control, format,
//! and DMA-address field lives directly in the card's own I/O space,
//! with no external chip to negotiate with. HDA instead splits into two
//! genuinely separate pieces talking to each other over a command/
//! response protocol:
//!
//! - **The controller** (this module): a PCI device exposing a small
//!   MMIO register set -- global reset/status, interrupt control, the
//!   CORB/RIRB command-transport DMA rings, and per-stream DMA
//!   descriptors (BDL) for real playback.
//! - **The codec**: a *separate* chip (on real hardware; QEMU emulates
//!   one directly) reachable only through 32-bit "verbs" the controller
//!   sends and 32-bit responses it reads back. A codec's own capabilities
//!   (how many output/input converters it has, which pins are wired to a
//!   physical jack, etc.) are not fixed by the HDA spec at all -- they
//!   must be discovered per-codec via `GET_PARAMETER`-style verbs, unlike
//!   every other driver in this project, which already knows its exact
//!   register/DMA shape from a fixed vendor/device ID match alone.
//!
//! **CORB/RIRB, not the Immediate Command interface.** The prior
//! discover-only step used ICH6+'s optional, simpler "Immediate Command"
//! path (`IC`/`IR`/`IRS`, one verb/response at a time, no DMA buffers
//! needed). That path is genuinely optional per the HDA spec -- real
//! hardware is not required to implement it -- while CORB (Command
//! Output Ring Buffer) and RIRB (Response Input Ring Buffer) are the
//! spec-mandatory transport every real controller must support. This
//! step switches entirely to CORB/RIRB, both because it is the
//! transport a genuine driver must ultimately rely on and because it is
//! the same DMA-ring pattern (host-memory ring, hardware-maintained
//! write pointer, software-maintained read pointer) real stream playback
//! will need later. Every CORB/RIRB register offset, bit, and DMA
//! read/write-pointer semantic below was cross-checked directly against
//! QEMU's own `hw/audio/intel-hda.c` (`intel_hda_corb_run`/
//! `intel_hda_response`, and the `regtab[]` register-definition table's
//! `reset`/`wmask` fields for `CORBSIZE`/`RIRBSIZE`/`CORBRP`/`RIRBWP`).
//!
//! **CORB/RIRB ring sizing is fixed, not negotiated.** `CORBSIZE`/
//! `RIRBSIZE` report (and, in QEMU, always reset to) `0x42`: bit 6 set
//! (256-entry size supported) and bits[1:0] = `10` (256 entries already
//! selected) -- QEMU's own register table gives these two registers no
//! `wmask` at all, meaning software cannot even attempt to select a
//! different size. The HDA spec does allow smaller sizes (2 or 16
//! entries) on some real controllers, selected by writing bits[1:0], but
//! since 256 is both the maximum and QEMU's only supported size, this
//! driver simply reads the register back after reset and trusts
//! whatever size it reports rather than attempting to write a
//! particular selection -- the safer assumption for real hardware that
//! might legitimately ignore an unsupported write the same way QEMU's
//! `wmask`-less registers do.
//!
//! **The CORB/RIRB read/write-pointer pre-increment convention is not
//! obvious from the register names alone**, and getting it backwards
//! silently reads/writes the wrong ring slot. Both `CORBWP` (last valid
//! CORB entry the *controller* has consumed) and `RIRBWP` (last valid
//! RIRB entry the controller has *written*) are "last written index"
//! registers, not "next free slot" -- so posting a new verb means
//! writing it to `(current_wp + 1) & 0xff` and *then* updating `CORBWP`
//! to that same new index (confirmed directly against
//! `intel_hda_corb_run`'s `rp = (d->corb_rp + 1) & 0xff` and
//! `intel_hda_response`'s identical `wp = (d->rirb_wp + 1) & 0xff`
//! pre-increment on the response side). Software has no separate
//! "RIRB read pointer" register at all -- only `RIRBWP` exists in
//! hardware -- so this driver tracks its own last-consumed RIRB index
//! (`Hda::rirb_rp`, a `Cell<u16>` since every send-and-wait call needs to
//! both read and advance it through a shared `&Hda`) and polls `RIRBWP`
//! for it to advance to `(rirb_rp + 1) & 0xff` before reading that slot.
//!
//! **Node-graph/widget enumeration.** Real hardware's node graph varies
//! by codec chip; this step walks it generically rather than assuming
//! any particular shape: read the root node's `NODE_COUNT` parameter to
//! find its subordinate nodes, read each one's `FUNCTION_TYPE` to find
//! the Audio Function Group (`AC_GRP_AUDIO_FUNCTION`, type `0x01`), read
//! the AFG's own `NODE_COUNT` to find its widget range, then for every
//! widget read `AUDIO_WIDGET_CAP` (extracting its type via
//! `AC_WCAP_TYPE_SHIFT`) and, for pin-complex widgets specifically,
//! `PIN_CAP` (output/input capability) and -- for any widget reporting a
//! connection list -- `CONNLIST_LEN` then `GET_CONNECT_LIST` for its
//! first (short-form) connection entry, showing which converter a pin is
//! wired to. Verified against QEMU's synthetic `hda-output`/`hda-duplex`
//! codecs' known layout (`hw/audio/hda-codec-common.h`): root (nid 0) ->
//! AFG (nid 1) -> DAC (nid 2) + line-out pin (nid 3, connection `{2}`),
//! plus, for duplex, ADC (nid 4) + line-in pin (nid 5, connection `{4}`).
//!
//! Every register offset, bit, and verb constant below was cross-checked
//! directly against QEMU's own `hw/audio/intel-hda.c`,
//! `hw/audio/intel-hda-defs.h`, and `hw/audio/hda-codec.c`/
//! `hda-codec-common.h` (pinned to this project's installed QEMU version,
//! 10.2.1) -- the same rigor this project applies to every other
//! register-level driver (see AGENTS.md's "Real hardware notes").
//! **Unverified against real hardware** -- expect the same class of
//! real-hardware fixups every other PCI driver here has needed.

use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::audio;
use crate::interrupts;
use crate::paging;
use crate::pci;
use crate::pmem;

/// QEMU's `intel-hda` (ich6, device ID `0x2668`) and `ich9-intel-hda`
/// (device ID `0x293e`) are both Intel-vendored and both fully emulated
/// in this project's installed QEMU, so both are matched here. A real
/// motherboard's onboard codec/controller may report a different Intel
/// device ID (or a different vendor entirely, e.g. AMD/Nvidia/VIA south
/// bridges also implement HDA controllers) -- confirm via the `pci`
/// shell command before assuming this driver claims a given real device,
/// the same caution this project already documents for every other
/// driver (see `es1370.rs`'s own doc comment). `DEVICE_CPT` (`0x1c20`) is
/// one such confirmed real ID: the Intel 6-series/Cougar Point PCH's HDA
/// controller, read via the `pci` command against the same physical ITX
/// motherboard `e1000e.rs` targets (class `04/03`, a real, non-zero
/// 64-bit MMIO BAR -- unlike the P4/ICH4 AC'97 investigation's hard-zero
/// BARs, nothing here suggests the controller itself is unpopulated;
/// whether a codec actually answers on its private bus is what this
/// addition is meant to confirm).
const VENDOR_INTEL: u16 = 0x8086;
const DEVICE_ICH6: u16 = 0x2668;
const DEVICE_ICH9: u16 = 0x293e;
const DEVICE_CPT: u16 = 0x1c20;

/// The controller's whole register block (including the stream
/// descriptors a future playback step would use) fits in one fixed
/// 0x4000-byte MMIO window, matching QEMU's own `intel_hda_realize`
/// (`memory_region_init(&d->container, ..., 0x4000)`).
const MMIO_BYTES: u32 = 0x4000;

// Register offsets, transcribed verbatim from `intel-hda-defs.h`'s
// `ICH6_REG_*` constants.
const REG_GCAP: u32 = 0x00;
const REG_GCTL: u32 = 0x08;
const REG_STATESTS: u32 = 0x0e;
const REG_CORBLBASE: u32 = 0x40;
const REG_CORBUBASE: u32 = 0x44;
const REG_CORBWP: u32 = 0x48;
const REG_CORBRP: u32 = 0x4a;
const REG_CORBCTL: u32 = 0x4c;
const REG_CORBSIZE: u32 = 0x4e;
const REG_RIRBLBASE: u32 = 0x50;
const REG_RIRBUBASE: u32 = 0x54;
const REG_RIRBWP: u32 = 0x58;
const REG_RINTCNT: u32 = 0x5a;
const REG_RIRBCTL: u32 = 0x5c;
const REG_RIRBSIZE: u32 = 0x5e;

/// `GCTL`'s controller-reset bit: 0 = controller held in reset, 1 =
/// normal operation. Software resets by clearing it, then setting it
/// again once ready to proceed -- the bit itself is the live reset
/// state, not a self-clearing "reset now" pulse.
const GCTL_CRST: u32 = 1 << 0;

/// `STATESTS` is 15 bits wide (one per possible codec address, `cad`
/// 0-14); bit `n` set means codec address `n` identified itself after
/// the most recent reset.
const STATESTS_MASK: u16 = 0x7fff;
const MAX_CODEC_ADDRESS: u32 = 15;

/// `GCAP`'s input/output stream-count fields -- not used for command
/// transport at all, but read once here (and reported by the `hda`
/// shell command) since a future playback step needs to know how many
/// output streams exist and where they start (`ISS` input streams occupy
/// the low stream indices, `OSS` output streams the ones right after).
const GCAP_ISS_SHIFT: u32 = 8;
const GCAP_ISS_MASK: u32 = 0xf;
const GCAP_OSS_SHIFT: u32 = 12;
const GCAP_OSS_MASK: u32 = 0xf;

// Both rings hold 256 entries once selected; `CORBSIZE`/`RIRBSIZE` are
// read-only-in-practice on QEMU (no `wmask`) and this driver does not
// attempt to write a different selection -- see this module's doc
// comment. `setup_corb_rirb` confirms the 256-entry size was actually
// selected by reading these registers back rather than hardcoding a
// ring-entry-count constant here.

/// `CORBRP`'s read-pointer-reset bit: software sets it, waits for the
/// readback to reflect it, clears it, then waits for the readback to
/// clear too -- the two-phase handshake real controllers use (mirrored
/// from Linux's `azx_reset`), even though QEMU's own emulation applies
/// either direction instantaneously.
const CORBRP_RST: u16 = 1 << 15;
/// `CORBCTL`'s DMA-enable bit.
const CORBCTL_RUN: u8 = 1 << 1;
/// `RIRBWP`'s write-pointer-reset bit: a self-clearing pulse (confirmed
/// against `intel_hda_set_rirb_wp`, which zeroes `rirb_wp` the instant
/// this bit is observed set) -- unlike `CORBRP_RST`, no readback polling
/// is needed here.
const RIRBWP_RST: u16 = 1 << 15;
/// `RIRBCTL`'s DMA-enable bit.
const RIRBCTL_DMA_EN: u8 = 1 << 1;
/// `RINTCNT`'s maximum value (an 8-bit field): set once, at setup, so
/// CORB processing never pauses waiting for a response-count interrupt
/// this polling-only driver never services (see `intel_hda_corb_run`'s
/// `rirb_count == rirb_cnt` stall condition) -- comfortably higher than
/// this driver's own one-verb-at-a-time usage will ever reach between
/// polls.
const RINTCNT_MAX: u16 = 0xff;

/// `AC_VERB_PARAMETERS` (`intel-hda-defs.h`): a "12-bit verb ID / 8-bit
/// payload" verb reading one of a node's fixed capability parameters.
const AC_VERB_PARAMETERS: u32 = 0xf00;
/// `AC_VERB_GET_CONNECT_LIST`: reads up to four (short-form) connection
/// entries starting at the given index, packed one per byte in the
/// response.
const AC_VERB_GET_CONNECT_LIST: u32 = 0xf02;

const AC_PAR_VENDOR_ID: u32 = 0x00;
const AC_PAR_NODE_COUNT: u32 = 0x04;
const AC_PAR_FUNCTION_TYPE: u32 = 0x05;
const AC_PAR_AUDIO_WIDGET_CAP: u32 = 0x09;
/// "Supported PCM Size, Rates" -- a converter (DAC/ADC) widget's own
/// capability parameter, queried only for `WidgetType::AudioOutput`/
/// `AudioInput` widgets (see `enumerate_afg_widgets`). Response bit
/// layout per the HDA spec: bits 0-10 are named rates (bit 5 = 44.1 kHz,
/// bit 6 = 48 kHz -- see `PCM_RATE_48000`; other rate bits are unused by
/// this driver), bits 16-20 are supported bit depths (bit 17 = 16-bit,
/// bit 19 = 24-bit -- see `PCM_BITS_24`). Unlike `AC_WCAP_CHAN_CNT_EXT`
/// (cross-checked against QEMU's own `hda-codec-common.h`, see that
/// constant's doc comment), this specific bit layout has **not** been
/// independently re-verified against QEMU source or a primary-source HDA
/// spec copy for this project -- `negotiate_format`'s fallback to the
/// historical 44.1 kHz/16-bit baseline whenever a bit this driver checks
/// for isn't set is the safety net if this layout turns out to be wrong
/// for some codec.
const AC_PAR_PCM: u32 = 0x0a;
const AC_PAR_PIN_CAP: u32 = 0x0c;
const AC_PAR_CONNLIST_LEN: u32 = 0x0e;

/// `AC_PAR_PCM` bit 6: this widget supports a 48 kHz-based rate.
const PCM_RATE_48000: u32 = 1 << 6;
/// `AC_PAR_PCM` bit 19: this widget supports 24-bit samples.
const PCM_BITS_24: u32 = 1 << 19;

const AC_NODE_ROOT: u32 = 0x00;
/// `FUNCTION_TYPE`'s low byte identifies an Audio Function Group,
/// distinct from e.g. a modem function group a codec chip could also
/// expose.
const AC_GRP_AUDIO_FUNCTION: u32 = 0x01;

/// `AUDIO_WIDGET_CAP`'s widget-type field, extracted via
/// `AC_WCAP_TYPE_SHIFT`/`AC_WCAP_TYPE`.
const AC_WCAP_TYPE_SHIFT: u32 = 20;
const AC_WCAP_TYPE_MASK: u32 = 0xf;
const AC_WCAP_STEREO: u32 = 1 << 0;
const AC_WCAP_CONN_LIST: u32 = 1 << 8;
/// `AUDIO_WIDGET_CAP`'s 3-bit channel-count-extension field (bits[15:13]),
/// combined with `AC_WCAP_STEREO` per the HDA spec -- and Linux's own
/// `get_wcaps_channels()` (`include/sound/hdaudio.h`, mirrored verbatim
/// in `sound/hda/common/hda_local.h`): `max_channels = ((ext << 1) | 1) +
/// 1` once a widget's stereo bit is set (Linux hardcodes the OR'd `1`
/// here rather than re-reading the stereo bit, since the spec only
/// defines this field as meaningful for an already-stereo-capable
/// widget); a widget with the stereo bit clear is mono-only (`1`)
/// regardless of whatever this field happens to contain. See
/// `widget_max_channels`.
const AC_WCAP_CHAN_CNT_EXT_SHIFT: u32 = 13;
const AC_WCAP_CHAN_CNT_EXT_MASK: u32 = 0x7;

const AC_PINCAP_OUT: u32 = 1 << 4;
const AC_PINCAP_IN: u32 = 1 << 5;

/// `CONNLIST_LEN`'s entry-count field (bits[6:0]); a widget with a
/// connection list but zero entries reported here has nothing to
/// actually fetch via `GET_CONNECT_LIST`.
const AC_CLIST_LENGTH_MASK: u32 = 0x7f;

/// Widget type codes, transcribed from `intel-hda-defs.h`'s `AC_WID_*`
/// enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WidgetType {
    AudioOutput,
    AudioInput,
    AudioMixer,
    AudioSelector,
    Pin,
    Power,
    VolumeKnob,
    Beep,
    Vendor,
    /// A widget-type code this driver does not have a named variant
    /// for -- the HDA spec reserves most of the type space for future
    /// use, so this keeps enumeration working (rather than silently
    /// dropping the widget) on a real codec exposing one.
    Other(u8),
}

impl WidgetType {
    fn from_raw(raw: u8) -> WidgetType {
        match raw {
            0x0 => WidgetType::AudioOutput,
            0x1 => WidgetType::AudioInput,
            0x2 => WidgetType::AudioMixer,
            0x3 => WidgetType::AudioSelector,
            0x4 => WidgetType::Pin,
            0x5 => WidgetType::Power,
            0x6 => WidgetType::VolumeKnob,
            0x7 => WidgetType::Beep,
            0xf => WidgetType::Vendor,
            other => WidgetType::Other(other),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            WidgetType::AudioOutput => "audio output (converter/DAC)",
            WidgetType::AudioInput => "audio input (converter/ADC)",
            WidgetType::AudioMixer => "audio mixer",
            WidgetType::AudioSelector => "audio selector",
            WidgetType::Pin => "pin complex",
            WidgetType::Power => "power widget",
            WidgetType::VolumeKnob => "volume knob",
            WidgetType::Beep => "beep generator",
            WidgetType::Vendor => "vendor-specific",
            WidgetType::Other(_) => "unrecognized widget type",
        }
    }
}

/// One AFG child widget, as reported by `AUDIO_WIDGET_CAP` and (for a
/// pin complex, or any widget reporting a connection list) a couple of
/// follow-up verbs. This is diagnostic information only in this step --
/// no format/stream programming happens yet.
#[derive(Debug, Clone, Copy)]
pub struct WidgetInfo {
    pub nid: u8,
    pub widget_type: WidgetType,
    pub stereo: bool,
    /// This widget's own maximum supported channel count, per
    /// `widget_max_channels` -- `2` for an ordinary stereo widget, `1`
    /// for a mono-only one, or higher for a widget whose
    /// `AC_WCAP_CHAN_CNT_EXT` field reports surround-capable channel
    /// support (never verified beyond `2` against any QEMU-emulated
    /// codec available to this project -- see that function's own doc
    /// comment).
    pub max_channels: u8,
    pub pin_output_capable: bool,
    pub pin_input_capable: bool,
    /// The widget's first connection-list entry (which other widget's
    /// output feeds this one), if it reports having any connections at
    /// all. Only the short-form (one byte per entry) encoding is read,
    /// which is all QEMU's synthetic codecs ever use; a real codec
    /// reporting the long-form (16-bit) encoding or more than four
    /// entries would need a fuller parser than this diagnostic step
    /// implements.
    pub connection: Option<u8>,
    /// The raw `AC_PAR_PCM` response, queried only for
    /// `WidgetType::AudioOutput`/`AudioInput` widgets (`0` for every
    /// other widget type, and for a converter whose query went
    /// unanswered) -- see `negotiate_format`, which treats `0` the same
    /// as "nothing beyond the historical 44.1 kHz/16-bit baseline is
    /// known to be supported."
    pub pcm_formats: u32,
}

/// Bounded busy-poll ceiling used throughout this driver: waiting for
/// `GCTL_CRST` to read back, and waiting for `RIRBWP` to advance after
/// posting a CORB verb. Mirrors the same "poll a fixed, generous number
/// of times, then report failure instead of hanging forever" convention
/// `parallel.rs`'s `BUSY_POLL_ATTEMPTS` already established for a
/// different slow, real-hardware-driven handshake.
const POLL_ATTEMPTS: u32 = 100_000;

/// How long to let codecs identify themselves on `STATESTS` after
/// deasserting `GCTL_CRST`, in 100 Hz PIT ticks. The HDA spec's own
/// minimum is far shorter (521 ns), but this kernel has no sub-tick
/// timer; two ticks (~20 ms) is a generous, bounded stand-in, the same
/// "round up to what this kernel can actually measure" tradeoff already
/// used elsewhere (e.g. `debug.rs`'s reboot-delay wait).
const CODEC_ENUMERATION_WAIT_TICKS: u64 = 2;

/// How long to hold the controller in reset before deasserting it again,
/// in 100 Hz PIT ticks -- the HDA spec's own minimum is 100us ("for codec
/// PLL to settle"), far shorter than this kernel's coarsest timer
/// granularity; one tick (~10ms) is a generous, bounded stand-in. See
/// `reset_controller`'s own doc comment for why this exists.
const RESET_SETTLE_TICKS: u64 = 1;

/// The maximum number of AFG child widgets this driver records per
/// codec. Comfortably above what QEMU's synthetic `hda-output`/
/// `hda-duplex` codecs expose (2 or 4); a real codec with a larger
/// widget tree would simply have its later widgets left unreported by
/// this diagnostic-only step rather than overflowing anything.
pub const MAX_WIDGETS: usize = 32;

fn read_u32(base: u32, offset: u32) -> u32 {
    unsafe { read_volatile((base + offset) as *const u32) }
}

fn write_u32(base: u32, offset: u32, value: u32) {
    unsafe { write_volatile((base + offset) as *mut u32, value) }
}

fn read_u16(base: u32, offset: u32) -> u16 {
    unsafe { read_volatile((base + offset) as *const u16) }
}

fn write_u16(base: u32, offset: u32, value: u16) {
    unsafe { write_volatile((base + offset) as *mut u16, value) }
}

fn read_u8(base: u32, offset: u32) -> u8 {
    unsafe { read_volatile((base + offset) as *const u8) }
}

fn write_u8(base: u32, offset: u32, value: u8) {
    unsafe { write_volatile((base + offset) as *mut u8, value) }
}

/// Encodes a "12-bit verb ID / 8-bit payload" command word -- the form
/// every verb this driver sends uses (`GET_PARAMETER`,
/// `GET_CONNECT_LIST`).
fn verb(cad: u8, nid: u8, verb_id: u32, payload: u32) -> u32 {
    ((cad as u32) << 28) | ((nid as u32) << 20) | (verb_id << 8) | (payload & 0xff)
}

/// Encodes a "4-bit verb ID / 16-bit payload" command word --
/// `SET_STREAM_FORMAT`/`SET_AMP_GAIN_MUTE`'s own category, distinct from
/// `verb`'s "12-bit verb / 8-bit payload" one. Confirmed directly from
/// QEMU's `hda_audio_command` (`hw/audio/hda-codec.c`, pinned to this
/// project's installed 10.2.1): a command's low 20 bits are decoded as
/// 12-bit-verb/8-bit-payload only when bits[18:16] are all set (true for
/// every verb `verb()` already handles, e.g. `0xf00`/`0x705..0x70f`);
/// every other verb instead uses this narrower category, where only the
/// top nibble (bits[19:16]) selects the verb and the remaining 16 bits
/// are the payload in full.
fn verb16(cad: u8, nid: u8, verb_id: u32, payload: u32) -> u32 {
    ((cad as u32) << 28) | ((nid as u32) << 20) | ((verb_id & 0xf) << 16) | (payload & 0xffff)
}

/// `AC_VERB_SET_STREAM_FORMAT` (`verb16` category): programs a converter
/// widget's stream format, using the same `AC_FMT_*` bit encoding as the
/// controller's own per-stream `SD_FORMAT` register (see
/// `stream_format`'s own doc comment).
const AC_VERB_SET_STREAM_FORMAT: u32 = 0x2;
/// `AC_VERB_SET_AMP_GAIN_MUTE` (`verb16` category).
const AC_VERB_SET_AMP_GAIN_MUTE: u32 = 0x3;
/// `AC_VERB_SET_CHANNEL_STREAMID` (`verb` category): assigns a converter
/// widget to a stream tag and starting channel -- distinct from, and
/// always sent alongside, `AC_VERB_SET_CVT_CHAN_COUNT` below, which sets
/// how many channels that stream actually carries.
const AC_VERB_SET_CHANNEL_STREAMID: u32 = 0x706;
/// `AC_VERB_SET_PIN_WIDGET_CONTROL` (`verb` category).
const AC_VERB_SET_PIN_WIDGET_CONTROL: u32 = 0x707;
/// `AC_VERB_SET_CVT_CHAN_COUNT` (`verb` category, confirmed against
/// Linux's own `include/sound/hda_verbs.h`): a converter widget whose
/// `AC_WCAP_CHAN_CNT_EXT` reports more than the base stereo pair needs
/// this verb, payload `channels - 1`, to actually activate more than 2
/// of its channels -- separate from `SET_CHANNEL_STREAMID` above (stream
/// tag/starting channel only) and from `SET_STREAM_FORMAT` (which
/// encodes the channel count too, but per the spec does not by itself
/// activate extra converter channels beyond stereo).
const AC_VERB_SET_CVT_CHAN_COUNT: u32 = 0x72d;

const AC_AMP_SET_OUTPUT: u32 = 1 << 15;
const AC_AMP_SET_LEFT: u32 = 1 << 13;
const AC_AMP_SET_RIGHT: u32 = 1 << 12;
/// A comfortable, non-maximal gain step (of the amp's 7-bit `0..0x7f`
/// range), the same "don't blast a real speaker at full gain the first
/// time this runs" caution a real driver would take -- purely a driver
/// default, not a hardware requirement.
const AC_AMP_GAIN_DEFAULT: u32 = 0x50;
/// `PIN_WIDGET_CONTROL`'s output-enable bit.
const AC_PINCTL_OUT_EN: u32 = 1 << 6;

/// First stream descriptor's byte offset within the MMIO window, and the
/// fixed per-stream stride between descriptors -- transcribed from
/// `intel-hda-defs.h`'s `ICH6_REG_SD_BASE`/`ICH6_REG_SD_SIZE`, mirroring
/// `intel_hda.c`'s own `ST_REG(n, o) = 0x80 + n*0x20 + o` macro. The
/// first *output* stream's index is `input_stream_count` (from `GCAP`,
/// already read into `Hda::input_stream_count`), never a hardcoded `4`
/// -- a real controller can report a different input/output split.
const STREAM_BASE: u32 = 0x80;
const STREAM_STRIDE: u32 = 0x20;

fn stream_offset(index: u8) -> u32 {
    STREAM_BASE + (index as u32) * STREAM_STRIDE
}

/// Per-stream register offsets, relative to `stream_offset(n)`. `SD_CTL`
/// is a 32-bit register whose low 24 bits are the actual control bits
/// and whose top byte aliases `SD_STS` (a separate, write-1-to-clear
/// byte, standard HDA spec behavior mirrored by Linux's `azx_sd_writeb`)
/// -- every write through this driver's `write_sd_ctl` helper zeroes
/// that top byte in the value it sends, specifically so an unrelated
/// control-bit change (e.g. programming the stream tag) can never
/// accidentally acknowledge a pending completion status it didn't mean
/// to touch.
const SD_CTL: u32 = 0x00;
const SD_STS: u32 = 0x03;
const SD_CBL: u32 = 0x08;
const SD_LVI: u32 = 0x0c;
const SD_FORMAT: u32 = 0x12;
const SD_BDLPL: u32 = 0x18;
const SD_BDLPU: u32 = 0x1c;

const SD_CTL_STREAM_RESET: u32 = 1 << 0;
const SD_CTL_RUN: u32 = 1 << 1;
const SD_CTL_IOCE: u32 = 1 << 2;
const SD_CTL_STREAM_TAG_SHIFT: u32 = 20;
const SD_STS_BCIS: u8 = 1 << 2;

const REG_INTCTL: u32 = 0x20;
const REG_INTSTS: u32 = 0x24;
const INTCTL_GIE: u32 = 1 << 31;
const INTCTL_CIE: u32 = 1 << 30;

/// One 16-byte Buffer Descriptor List entry: a 64-bit physical address,
/// a 32-bit length in bytes, and a 32-bit flags word whose only defined
/// bit (bit0) requests an interrupt when the controller finishes this
/// entry -- transcribed from `intel_hda_parse_bdl`'s own byte offsets.
const BDL_ENTRY_BYTES: u32 = 16;
const BDL_FLAG_IOC: u32 = 1;

/// Matches ES1370's own period sizing (`es1370.rs`'s `PERIOD_FRAMES`),
/// for the same ~23 ms latency/jitter-tolerance tradeoff and so both
/// backends' `audio.rs` diagnostics stay directly comparable. Frame
/// *count* stays fixed regardless of the negotiated channel count below
/// -- only the per-frame byte size (and therefore each period's/the
/// whole buffer's byte size) scales with channels.
const PLAYBACK_PERIOD_FRAMES: u32 = 1024;
const PLAYBACK_PERIOD_COUNT: u32 = 2;
const PLAYBACK_TOTAL_FRAMES: u32 = PLAYBACK_PERIOD_FRAMES * PLAYBACK_PERIOD_COUNT;

/// This driver's own imposed ceiling on a negotiated channel count,
/// independent of whatever a widget's `AC_WCAP_CHAN_CNT_EXT` field would
/// otherwise allow (the stream format field itself could encode up to
/// 16 channels -- see `stream_format`'s own comment). `8` matches
/// the common real-world "7.1 surround" ceiling and keeps period/buffer
/// byte math comfortably bounded. Every QEMU-emulated HDA codec
/// available to this project (`hda-output`/`hda-duplex`/`hda-micro`)
/// reports `chan_cnt_ext = 0` on every widget -- confirmed directly from
/// `hw/audio/hda-codec-common.h` -- so negotiation always settles on `2`
/// channels in every automated test here; this ceiling, and the
/// above-`2` negotiation path in general, is implemented per the HDA
/// spec and Linux's own reference driver but can only be exercised
/// against real, genuinely surround-capable hardware.
const MAX_NEGOTIATED_CHANNELS: u8 = 8;

/// This driver's two supported sample bit depths. Real HDA silicon can
/// encode up to 32-bit samples in the stream format field (see
/// `format_bits_field`), but 16/24-bit is what this driver negotiates
/// and no more -- the same "don't build untestable generality" scope
/// choice this project already makes elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BitDepth {
    Bits16,
    Bits24,
}

impl BitDepth {
    /// Bytes occupied by one sample container in the DMA buffer.
    /// 16-bit is unambiguous (2 tightly-packed bytes). 24-bit is **not**
    /// tightly packed to 3 bytes -- this driver assumes the common
    /// "24 significant bits left-justified in a 4-byte container"
    /// convention (matching Linux ALSA's `S24_LE` format), the same way
    /// most real-world 24-bit audio hardware/drivers do it, but this has
    /// never been verified against a primary-source HDA spec copy or
    /// real hardware: QEMU's synthetic HDA codec only ever implements
    /// 16-bit PCM (`QEMU_HDA_PCM_FORMATS` in `hda-codec-common.h`), so
    /// this path cannot be exercised at all under this project's only
    /// automated test environment. Treat this as a documented
    /// assumption needing real-hardware confirmation, not a proven fact
    /// -- see AGENTS.md's HDA notes.
    fn bytes_per_sample(self) -> u32 {
        match self {
            BitDepth::Bits16 => 2,
            BitDepth::Bits24 => 4,
        }
    }

    /// The stream format field's bits[6:4]: `001` for 16-bit, `011` for
    /// 24-bit, per the HDA Converter Format encoding.
    fn format_bits_field(self) -> u32 {
        match self {
            BitDepth::Bits16 => 0b001,
            BitDepth::Bits24 => 0b011,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            BitDepth::Bits16 => 16,
            BitDepth::Bits24 => 24,
        }
    }
}

/// Picks this driver's preferred rate/bit-depth from a converter
/// widget's raw `AC_PAR_PCM` capability (see that constant's own doc
/// comment on the bit layout and its unverified status). Prefers 48 kHz
/// over 44.1 kHz and 24-bit over 16-bit, but only when the widget
/// actually advertises support -- `pcm_formats == 0` (query failed, or
/// never issued for a non-converter widget) falls back to this driver's
/// original, proven-working 44.1 kHz/16-bit baseline, and so does any
/// widget that simply doesn't set the bits this driver looks for. This
/// fallback is what makes the (unverified) `AC_PAR_PCM` bit layout safe
/// to guess at: worst case, a wrong guess just never selects 48 kHz/
/// 24-bit, not a wrong/corrupt format.
fn negotiate_format(pcm_formats: u32) -> (bool, BitDepth) {
    let base_44100 = pcm_formats & PCM_RATE_48000 == 0;
    let bits = if pcm_formats & PCM_BITS_24 != 0 {
        BitDepth::Bits24
    } else {
        BitDepth::Bits16
    };
    (base_44100, bits)
}

/// `AC_FMT_*`-encoded stream format: bit14 selects the 44.1 kHz base
/// rate (clear selects 48 kHz instead), bits[6:4] select the bit depth
/// (see `BitDepth::format_bits_field`), bits[3:0] = channels-1. No
/// divisor/multiplier (fields left at 0) -- this driver only ever
/// requests a widget's two named base rates directly, never a derived
/// multiple. `channels` must already be clamped to
/// `1..=MAX_NEGOTIATED_CHANNELS` by the caller (every call site below
/// negotiates through `widget_max_channels` and this ceiling first), so
/// the low nibble can never overflow past the format field's own 4-bit
/// width.
fn stream_format(base_44100: bool, bits: BitDepth, channels: u8) -> u32 {
    let rate_bit = if base_44100 { 0x4000 } else { 0 };
    let bits_field = bits.format_bits_field() << 4;
    rate_bit | bits_field | ((channels as u32).saturating_sub(1) & 0xf)
}

/// Fixed, arbitrary non-zero stream tag; only one playback stream is
/// ever active per controller in this driver, so no allocation scheme is
/// needed -- any codec's `SET_CHANNEL_STREAMID` and this controller's own
/// `SD_CTL` stream-tag field just need to agree with each other.
const PLAYBACK_STREAM_TAG: u32 = 1;

/// The capture (ADC/input) stream's own fixed stream tag. Deliberately
/// the same numeric value as `PLAYBACK_STREAM_TAG`: stream tags are only
/// ever compared against the `SD_CTL` register of *one particular*
/// stream descriptor (a converter widget's `SET_CHANNEL_STREAMID` is
/// matched against its own direction's stream, per the HDA spec), so an
/// output tag and an input tag reusing the same small integer can never
/// collide -- exactly like `AC_AMP_SET_OUTPUT`/`AC_AMP_SET_INPUT` below
/// being independent despite sharing the rest of that verb's payload
/// bits.
const CAPTURE_STREAM_TAG: u32 = 1;

/// `AC_VERB_SET_AMP_GAIN_MUTE`'s "Set Input Amp" flag -- the capture-path
/// counterpart to `AC_AMP_SET_OUTPUT`, selecting the ADC converter's
/// input amplifier stage instead of a DAC/pin's output stage.
const AC_AMP_SET_INPUT: u32 = 1 << 14;

/// `PIN_WIDGET_CONTROL`'s input-enable bit -- the capture-path
/// counterpart to `AC_PINCTL_OUT_EN`, at the adjacent bit position (bit 5
/// vs. bit 6), matching `AC_PINCAP_IN`/`AC_PINCAP_OUT`'s own analogous
/// bit-position relationship in the pin's *capability* register.
const AC_PINCTL_IN_EN: u32 = 1 << 5;

/// This driver's capture buffer/period sizing reuses the same fixed
/// frame count as playback (`PLAYBACK_PERIOD_FRAMES`/
/// `PLAYBACK_PERIOD_COUNT`) -- mirroring `es1370.rs`'s own
/// `PERIOD_FRAMES`/`BUFFER_BYTES`, which are likewise shared unchanged
/// between its DAC1 and ADC paths -- but, since HDA multichannel
/// negotiation (see `widget_max_channels`) is independent per converter
/// widget, playback and capture may each negotiate a different channel
/// count and therefore a different per-frame/period/buffer byte size;
/// `Hda::playback_channels`/`Hda::capture_channels` track this
/// separately rather than assuming the two directions always agree.

#[derive(Debug, Clone, Copy)]
pub enum Status {
    NotPresent,
    InitFailed,
}


/// A codec address (`cad`) that responded during discovery: its root
/// node's reported Vendor/Device ID, which of its subordinate nodes is
/// the Audio Function Group (if any was found), and that AFG's own
/// child widgets.
#[derive(Debug, Clone, Copy)]
pub struct CodecInfo {
    pub address: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub afg_nid: Option<u8>,
    widgets: [Option<WidgetInfo>; MAX_WIDGETS],
    widget_count: usize,
}

impl CodecInfo {
    pub fn widgets(&self) -> &[Option<WidgetInfo>] {
        &self.widgets[..self.widget_count]
    }
}

const MAX_CODECS: usize = MAX_CODEC_ADDRESS as usize;

/// How many independent HDA controllers this driver will discover and
/// drive simultaneously -- mirroring `es1370.rs`'s own `MAX_INSTANCES`,
/// now that a real need for more than one controller has appeared (a
/// real machine can easily have one onboard controller plus a discrete
/// add-in card). Deliberately smaller than ES1370's `4`: unlike ES1370,
/// where each instance is a fixed one-DAC-per-card stereo pair (so
/// reaching a target channel count meant multiplying instances), one HDA
/// codec can already expose several independent DAC/pin pairs on its own
/// node graph once playback is implemented, so this ceiling only needs
/// to cover a plausible number of physically installed *controllers*,
/// not a channel-count multiplier. `2` also matches the existing
/// `find_devices` match-array size below, which was already sized for
/// "at most a couple of these on one machine" before this change.
pub const MAX_INSTANCES: usize = 2;

pub struct Hda {
    /// Which slot in `CONTROLLERS` this instance occupies -- reported by
    /// `hda_status` so each discovered controller's output is clearly
    /// distinguished, mirroring `Es1370`'s own `instance` field.
    instance: usize,
    mmio_base: u32,
    irq: Option<u8>,
    /// The MSI vector this controller is actually using for interrupt
    /// delivery, if `init_controller` was able to enable MSI for it (see
    /// "MSI/MSI-X" in AGENTS.md). `irq` above still reflects whatever
    /// legacy line firmware assigned regardless -- kept purely for
    /// diagnostics -- but `handle_pci_irq`'s dispatch match and the
    /// registration call sites below only ever use one or the other, never
    /// both, for a given controller.
    msi_vector: Option<u8>,
    corb_base: u32,
    rirb_base: u32,
    /// Software's own tracked "last consumed RIRB entry" index -- see
    /// this module's doc comment for why no such register actually
    /// exists in hardware. `Cell` because every `send_corb_command` call
    /// through a shared `&Hda` both reads and advances it.
    rirb_rp: Cell<u16>,
    input_stream_count: u8,
    output_stream_count: u8,
    codecs: [Option<CodecInfo>; MAX_CODECS],
    codec_count: usize,
    /// Which output stream index (a `GCAP`-derived, controller-relative
    /// index -- see `STREAM_BASE`'s own doc comment) is programmed for
    /// playback, if `find_playback_target` located a usable DAC/pin pair
    /// and this controller has at least one output stream at all.
    /// `None` means this controller has no active playback -- every
    /// `audio.rs`-facing method below is a no-op in that case, and
    /// `init_controller` never adds such a controller to `audio::DEVICES`
    /// (see `Hda::has_playback`).
    stream_index: Option<u8>,
    /// The negotiated playback channel count -- see `widget_max_channels`
    /// and `setup_playback`'s own doc comment. Always `2` (this driver's
    /// original fixed value) when `stream_index` is `None`, since nothing
    /// ever negotiated a real value in that case; `format()` and every
    /// buffer/period byte-size calculation read this field rather than a
    /// fixed constant.
    playback_channels: u8,
    /// The negotiated sample rate/bit depth, shared by both directions
    /// exactly like `format()` only ever reporting `playback_channels`
    /// (see that method's own doc comment) -- set once by
    /// `setup_playback` via `negotiate_format`, or left at this driver's
    /// original, proven-working 44.1 kHz/16-bit baseline if no playback
    /// stream is active (matching `negotiate_format(0)`'s own fallback,
    /// since nothing ever negotiated a real value in that case).
    sample_rate: u32,
    bits_per_sample: BitDepth,
    /// Physically contiguous DMA region backing the playback ring (two
    /// periods, `playback_buffer_bytes()` total) -- the codec itself
    /// pulls bytes from here via its own DMA, unlike ES1370's
    /// controller-side FRAMEADR/FRAMECNT pair (see this module's doc
    /// comment on why HDA's DMA model is codec-pulled, not
    /// controller-pushed).
    buffer_addr: u32,
    interrupt_count: AtomicU32,
    /// Counts every serviced playback period since streaming started;
    /// also used, mod `PLAYBACK_PERIOD_COUNT`, to pick which half of the
    /// ring to refill next -- exactly the same software-tracked-position
    /// convention `es1370::Es1370::periods_serviced` already uses, for
    /// the identical reason: hardware loops through the BDL entries
    /// entirely on its own once started, so no position readback is
    /// needed to know which half just finished.
    periods_serviced: AtomicU32,
    /// Set permanently on this controller's first `submit` call -- see
    /// `es1370::Es1370::network_mode`'s own doc comment for why an empty
    /// ring means silence afterward, never a fallback to the built-in
    /// test tone.
    network_mode: AtomicBool,
    underrun_count: AtomicU32,
    /// The capture-path counterpart to `stream_index`: which input
    /// stream index is programmed for recording, if `find_capture_target`
    /// located a usable ADC/pin pair and this controller has at least one
    /// input stream at all. `None` means no active capture -- every
    /// capture-facing method below is a no-op in that case, mirroring
    /// `stream_index`/`has_playback`'s own convention exactly.
    capture_stream_index: Option<u8>,
    /// The capture-path counterpart to `playback_channels`: the
    /// negotiated capture channel count, independent of whatever
    /// playback negotiated (a codec's ADC and DAC widgets need not report
    /// the same `AC_WCAP_CHAN_CNT_EXT`). `audio.rs`'s single, shared
    /// `format()` method only ever reports `playback_channels` -- see
    /// that method's own doc comment for why a tape reading captured
    /// bytes via `audio_capture`/`_at` should not assume they necessarily
    /// match `format()`'s reported channel count on a codec where the two
    /// directions genuinely differ.
    capture_channels: u8,
    /// The capture-path counterpart to `buffer_addr`: a separate,
    /// physically contiguous DMA region the codec's ADC converter writes
    /// captured samples into.
    capture_buffer_addr: u32,
    /// The capture-path counterpart to `periods_serviced`.
    capture_periods_serviced: AtomicU32,
    /// Incremented whenever a just-drained capture period couldn't be
    /// fully queued into `CAPTURE_RINGS` (ring already full) -- the
    /// capture-path counterpart to `underrun_count`, mirroring
    /// `es1370::Es1370::capture_overrun_count`.
    capture_overrun_count: AtomicU32,
}

impl Hda {
    pub fn instance(&self) -> usize {
        self.instance
    }

    pub fn mmio_base(&self) -> u32 {
        self.mmio_base
    }

    pub fn irq(&self) -> Option<u8> {
        self.irq
    }

    pub fn msi_vector(&self) -> Option<u8> {
        self.msi_vector
    }

    pub fn input_stream_count(&self) -> u8 {
        self.input_stream_count
    }

    pub fn output_stream_count(&self) -> u8 {
        self.output_stream_count
    }

    pub fn codecs(&self) -> &[Option<CodecInfo>] {
        &self.codecs[..self.codec_count]
    }

    /// Whether this controller has a usable, already-programmed
    /// playback stream -- `audio.rs`'s facade only ever wraps a
    /// controller that reports `true` here in an `Audio::Hda(...)`
    /// device.
    pub fn has_playback(&self) -> bool {
        self.stream_index.is_some()
    }

    /// Whether this controller has a usable, already-programmed capture
    /// (ADC) stream -- unlike `has_playback`, this never gates whether
    /// the controller is added to `audio::DEVICES` at all (a controller
    /// with playback but no capture, or vice versa, is still a perfectly
    /// usable device for whichever direction it does support); it exists
    /// purely so `audio.rs`'s facade methods below can no-op cleanly for
    /// a controller that never found a usable ADC/pin pair.
    pub fn has_capture(&self) -> bool {
        self.capture_stream_index.is_some()
    }

    /// This device's negotiated native playback format: `sample_rate`/
    /// `bits_per_sample` reflect whatever `setup_playback` negotiated via
    /// `negotiate_format` against the real DAC widget's `AC_PAR_PCM`
    /// capability (44.1 kHz/16-bit if no playback stream is active at
    /// all, or if the widget didn't advertise anything beyond that
    /// baseline -- every codec this project's QEMU tests use, since
    /// QEMU's synthetic HDA codec only ever implements 16-bit PCM), with
    /// `playback_channels` reflecting whatever `setup_playback` actually
    /// negotiated against the real DAC/pin widget pair (`2` if no
    /// playback stream is active, or if negotiation ran against a codec
    /// reporting no `AC_WCAP_CHAN_CNT_EXT` capability). This is the one
    /// format the tape ABI's `audio_channels_and_bits`/`_at` callbacks
    /// report for *both* directions of this device; see
    /// `capture_channels`'s own doc comment for the accepted limitation
    /// this implies on a codec whose ADC and DAC negotiate differently.
    pub fn format(&self) -> audio::AudioFormat {
        audio::AudioFormat {
            sample_rate: self.sample_rate,
            channels: self.playback_channels,
            bits_per_sample: self.bits_per_sample.as_u8(),
        }
    }

    /// `playback_channels` * the negotiated bit depth's byte size, the
    /// per-frame byte size `fill_period` computes its period byte size
    /// from (`setup_playback` computes this and the buffer/period byte
    /// sizes itself, once, at negotiation time, since they're also
    /// needed there before `playback_channels`/`bits_per_sample` are even
    /// set).
    fn playback_bytes_per_frame(&self) -> u32 {
        self.playback_channels as u32 * self.bits_per_sample.bytes_per_sample()
    }

    fn playback_period_bytes(&self) -> u32 {
        PLAYBACK_PERIOD_FRAMES * self.playback_bytes_per_frame()
    }

    /// The capture-path counterpart to `playback_bytes_per_frame`. Shares
    /// `bits_per_sample` with the playback path (see that field's own
    /// doc comment on why capture doesn't negotiate its own bit depth
    /// independently).
    fn capture_bytes_per_frame(&self) -> u32 {
        self.capture_channels as u32 * self.bits_per_sample.bytes_per_sample()
    }

    fn capture_period_bytes(&self) -> u32 {
        PLAYBACK_PERIOD_FRAMES * self.capture_bytes_per_frame()
    }

    /// Accepts decoded PCM bytes from a tape (via the ABI's
    /// `audio_submit`/`audio_submit_at` callbacks), in this device's own
    /// `format()`, and switches this controller permanently into
    /// network-fed mode on first use -- mirrors `Es1370::submit` exactly.
    pub fn submit(&self, data: &[u8]) -> usize {
        self.network_mode.store(true, Ordering::Relaxed);
        RINGS[self.instance].push(data)
    }

    pub fn underrun_count(&self) -> u32 {
        self.underrun_count.load(Ordering::Relaxed)
    }

    pub fn ring_backlog(&self) -> usize {
        RINGS[self.instance].available()
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32) {
        (
            self.irq,
            self.interrupt_count.load(Ordering::Relaxed),
            self.periods_serviced.load(Ordering::Relaxed),
        )
    }

    /// Pulls up to `out.len()` captured bytes (in this device's own
    /// `format()`) out of `CAPTURE_RINGS`, returning how many bytes were
    /// actually copied -- mirrors `Es1370::capture` exactly, including
    /// its "however much is ready" (never blocking, never short of a
    /// full period) semantics. Always `0` for a controller with no
    /// usable capture stream (`has_capture()` false).
    pub fn capture(&self, out: &mut [u8]) -> usize {
        if !self.has_capture() {
            return 0;
        }
        unsafe { CAPTURE_RINGS[self.instance].pop_upto(out.as_mut_ptr(), out.len()) }
    }

    pub fn capture_overrun_count(&self) -> u32 {
        self.capture_overrun_count.load(Ordering::Relaxed)
    }

    /// Bytes currently queued in this instance's own `CAPTURE_RINGS`
    /// slot, the capture-side counterpart to `ring_backlog`.
    pub fn capture_backlog(&self) -> usize {
        CAPTURE_RINGS[self.instance].available()
    }

    pub fn capture_periods(&self) -> u32 {
        self.capture_periods_serviced.load(Ordering::Relaxed)
    }

    /// Raw read of this controller's global interrupt-status register,
    /// exposed for diagnostics -- the `Hda` counterpart to
    /// `Es1370::raw_status`.
    pub fn raw_status(&self) -> u32 {
        read_u32(self.mmio_base, REG_INTSTS)
    }

    /// This device's base address for `audio.rs`'s facade-level
    /// diagnostics -- an MMIO physical address, unlike `Es1370::io_base`'s
    /// legacy I/O port number, but reported through the same facade
    /// method (widened to `u32`; see `Audio::io_base`'s own doc comment).
    pub fn io_base(&self) -> u32 {
        self.mmio_base
    }

    /// Fills one period's worth of the playback DMA ring, exactly like
    /// `Es1370::fill_period`: pulls a full period from `RINGS` once
    /// `network_mode` is active, falling back to silence on an underrun;
    /// before that, plays nothing but silence (unlike ES1370, this
    /// driver has no built-in test-tone generator of its own, since the
    /// codec's own DMA pulls whatever this buffer holds regardless of
    /// whether a tape has ever submitted anything -- silence is the
    /// correct, harmless idle state here).
    fn fill_period(&self, half: u32) {
        let offset = half * self.playback_period_bytes();
        let base = (self.buffer_addr + offset) as *mut u8;
        let period_bytes = self.playback_period_bytes() as usize;

        if self.network_mode.load(Ordering::Relaxed) {
            let filled = unsafe { RINGS[self.instance].pop_into(base, period_bytes) };
            if filled {
                return;
            }
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
        }
        for i in 0..period_bytes {
            unsafe {
                core::ptr::write_volatile(base.add(i), 0u8);
            }
        }
    }

    /// The capture-path counterpart to `fill_period`: copies one
    /// just-completed period's worth of captured PCM bytes out of the
    /// ADC's own DMA ring and pushes them into `CAPTURE_RINGS` for a tape
    /// to later drain via `capture`/the ABI's `audio_capture_at`
    /// callback. Unlike `fill_period`, there is no fallback content to
    /// generate here -- captured samples either exist or they don't; a
    /// full `CAPTURE_RINGS` slot (a tape not draining fast enough) is
    /// reported via `capture_overrun_count` and the remaining, unqueued
    /// bytes are simply dropped, mirroring
    /// `Es1370::drain_capture_period` exactly.
    fn drain_capture_period(&self, half: u32) {
        let offset = half * self.capture_period_bytes();
        let base = (self.capture_buffer_addr + offset) as *const u8;
        let period_bytes = self.capture_period_bytes() as usize;
        let captured = unsafe { core::slice::from_raw_parts(base, period_bytes) };
        let accepted = CAPTURE_RINGS[self.instance].push(captured);
        if accepted < captured.len() {
            self.capture_overrun_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The 32-bit register at `SD_CTL`'s offset packs the actual control
    /// bits (low 24) and the write-1-to-clear `SD_STS` byte (top 8) into
    /// one dword -- see `SD_CTL`'s own doc comment for why every write
    /// through here zeroes that top byte, so a control-bit change never
    /// accidentally acknowledges a completion status it wasn't meant to
    /// touch. Takes an explicit stream index (rather than assuming
    /// `self.stream_index`) so both the playback and capture paths can
    /// share this one helper.
    fn write_sd_ctl(&self, stream_index: u8, ctl_bits: u32) {
        let offset = stream_offset(stream_index) + SD_CTL;
        write_u32(self.mmio_base, offset, ctl_bits & 0x00ff_ffff);
    }

    fn read_sd_ctl(&self, stream_index: u8) -> u32 {
        let offset = stream_offset(stream_index) + SD_CTL;
        read_u32(self.mmio_base, offset) & 0x00ff_ffff
    }

    /// Interrupt handler for this controller's playback and capture
    /// streams, called from `handle_pci_irq` for every registered
    /// controller sharing the firing IRQ line (see that function's own
    /// doc comment for why checking this controller's own status bits
    /// first makes that safe). Checks each direction's `SD_STS`
    /// unconditionally rather than as an if/else -- exactly like
    /// `Es1370::handle_interrupt` checking `STAT_DAC1`/`STAT_ADC`
    /// independently -- since both streams can legitimately complete a
    /// period on the same interrupt. Refills/drains whichever ring half
    /// just finished and acknowledges the completion the same
    /// write-1-to-clear way real HDA hardware requires -- unlike
    /// ES1370's level-triggered `SERIAL_CONTROL` toggle, this is a
    /// genuine per-bit clear, so no read-modify-write dance is needed.
    pub fn handle_interrupt(&self) {
        if let Some(stream_index) = self.stream_index {
            let sts_offset = stream_offset(stream_index) + SD_STS;
            let status = read_u8(self.mmio_base, sts_offset);
            if status & SD_STS_BCIS != 0 {
                self.interrupt_count.fetch_add(1, Ordering::Relaxed);
                let served = self.periods_serviced.fetch_add(1, Ordering::Relaxed);
                self.fill_period(served % PLAYBACK_PERIOD_COUNT);
                write_u8(self.mmio_base, sts_offset, SD_STS_BCIS);
            }
        }
        if let Some(stream_index) = self.capture_stream_index {
            let sts_offset = stream_offset(stream_index) + SD_STS;
            let status = read_u8(self.mmio_base, sts_offset);
            if status & SD_STS_BCIS != 0 {
                self.interrupt_count.fetch_add(1, Ordering::Relaxed);
                let captured = self.capture_periods_serviced.fetch_add(1, Ordering::Relaxed);
                self.drain_capture_period(captured % PLAYBACK_PERIOD_COUNT);
                write_u8(self.mmio_base, sts_offset, SD_STS_BCIS);
            }
        }
    }

    /// Sends one verb through the CORB/RIRB DMA transport and returns
    /// its response, or `None` if `RIRBWP` never advanced to the
    /// expected next slot within this driver's bounded poll -- treated
    /// throughout this module as "that node/codec didn't answer",
    /// exactly like the prior step's Immediate-Command failure case.
    fn send_corb_command(&self, verb: u32) -> Option<u32> {
        // CORBWP, like RIRBWP, is a "last written index" register: the
        // new verb goes to the *next* slot, then the register is
        // updated to that same new index -- see this module's doc
        // comment for why getting this backwards would silently target
        // the wrong ring slot.
        let wp = (read_u16(self.mmio_base, REG_CORBWP).wrapping_add(1)) & 0xff;
        unsafe {
            write_volatile((self.corb_base + 4 * wp as u32) as *mut u32, verb);
        }
        write_u16(self.mmio_base, REG_CORBWP, wp);

        let target = (self.rirb_rp.get().wrapping_add(1)) & 0xff;
        if !poll_until(|| read_u16(self.mmio_base, REG_RIRBWP) & 0xff == target) {
            return None;
        }
        let response = unsafe { read_volatile((self.rirb_base + 8 * target as u32) as *const u32) };
        self.rirb_rp.set(target);
        Some(response)
    }
}

static mut CONTROLLERS: [Option<Hda>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CONTROLLER_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

/// The kernel-owned jitter buffer each controller's playback stream
/// feeds via `Hda::submit`/the ABI's `audio_submit`/`audio_submit_at`
/// callbacks -- one slot per possible controller, indexed by
/// `Hda::instance`, mirroring `es1370.rs`'s own `RINGS` array (see that
/// static's doc comment for why this lives as a standalone top-level
/// array rather than a field embedded in `Hda` itself: avoiding a large
/// stack copy while `init_controller`'s own `let mut hda = Hda { ... }`
/// is still a local).
static RINGS: [audio::PcmRing; MAX_INSTANCES] =
    [const { audio::PcmRing::new() }; MAX_INSTANCES];

/// The capture-path counterpart to `RINGS`: each controller's own ADC
/// interrupt handler (`Hda::drain_capture_period`) pushes just-captured
/// PCM bytes in here, and a tape drains them back out via the ABI's
/// `audio_capture`/`audio_capture_at` callbacks (`Hda::capture`) --
/// mirrors `es1370.rs`'s own `CAPTURE_RINGS` array exactly, including
/// living as a standalone top-level array for the same reason `RINGS`
/// does above.
static CAPTURE_RINGS: [audio::PcmRing; MAX_INSTANCES] =
    [const { audio::PcmRing::new() }; MAX_INSTANCES];

/// Shared legacy IRQs are normal (see `es1370.rs`'s own doc comment on
/// this exact point, and AGENTS.md's "A real bug this test found: shared
/// legacy PCI IRQ lines silently broke a second device's `init()`").
/// Each controller only reads and acknowledges its own `SD_STS` byte, so
/// calling every registered controller's `handle_interrupt` on every
/// fire is always safe, even for a controller that was not the actual
/// source this time (or has no playback stream programmed at all).
/// `irq_or_vector` is either a legacy IRQ number (0-15, via
/// `register_pci_irq`) or an MSI vector (`interrupts::MSI_VECTOR_BASE`
/// and up, via `register_msi_irq`) -- the two ranges never overlap, so
/// matching against both `irq`/`msi_vector` fields below is unambiguous.
/// A controller using MSI never shares its vector with anything else (see
/// `msi_vector`'s own doc comment), but the shared-legacy-line case this
/// function already handled before MSI existed is unchanged.
fn handle_pci_irq(irq_or_vector: u8) {
    unsafe {
        for controller in (*core::ptr::addr_of!(CONTROLLERS)).iter().flatten() {
            if controller.irq == Some(irq_or_vector) || controller.msi_vector == Some(irq_or_vector)
            {
                controller.handle_interrupt();
            }
        }
    }
}

/// Discovers and resets every usable HDA controller (up to
/// `MAX_INSTANCES`), sets up each one's own independent CORB/RIRB
/// command transport, enumerates whichever codec addresses respond on
/// it, and walks each responding codec's Audio Function Group/widget
/// tree -- mirroring `es1370::initialize_all`. Idempotent: a second call
/// after a first (successful or not) returns the already-discovered
/// count immediately without touching hardware again. Each controller's
/// own PCI BAR gives it a distinct `mmio_base`, so `paging::map_mmio`
/// and every register access below are already naturally
/// instance-scoped with no risk of one controller's rings/registers
/// colliding with another's.
pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if *core::ptr::addr_of!(INITIALIZED) {
            let count = *core::ptr::addr_of!(CONTROLLER_COUNT);
            return if count > 0 {
                Ok(count)
            } else {
                Err(Status::NotPresent)
            };
        }
        *core::ptr::addr_of_mut!(INITIALIZED) = true;
    }

    let mut matches: [Option<pci::PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_devices(
        VENDOR_INTEL,
        &[DEVICE_ICH6, DEVICE_ICH9, DEVICE_CPT],
        &mut matches,
    );
    if found == 0 {
        return Err(Status::NotPresent);
    }

    for address in matches[..found.min(MAX_INSTANCES)].iter().flatten() {
        let instance = unsafe { *core::ptr::addr_of!(CONTROLLER_COUNT) };
        if instance >= MAX_INSTANCES {
            break;
        }
        if let Ok(hda) = init_controller(*address, instance) {
            unsafe {
                (*core::ptr::addr_of_mut!(CONTROLLERS))[instance] = Some(hda);
                *core::ptr::addr_of_mut!(CONTROLLER_COUNT) = instance + 1;
            }
        }
    }

    let count = unsafe { *core::ptr::addr_of!(CONTROLLER_COUNT) };
    if count == 0 {
        Err(Status::InitFailed)
    } else {
        Ok(count)
    }
}

/// Brings up one controller at the given PCI address: forces D0 power
/// state, enables memory space/bus mastering, maps its MMIO BAR, resets
/// it, sets up its own CORB/RIRB rings, and enumerates its codecs. Split
/// out of `initialize_all` the same way `es1370::init_card` is split out
/// of its own `initialize_all`, so a failure on one controller (e.g. a
/// misbehaving second card) does not prevent an earlier one from being
/// used.
///
/// The `force_power_state_d0` call is a real-hardware fix (see
/// `pci::PciAddress::force_power_state_d0`'s own doc comment for the
/// general rationale, first added for `e1000e.rs`): an onboard
/// controller BIOS never actively uses -- exactly the case for HD Audio
/// on a board with no onboard-audio-dependent boot path -- can be left
/// in a low-power D-state that `reset_controller` alone cannot recover
/// from.
fn init_controller(address: pci::PciAddress, instance: usize) -> Result<Hda, Status> {
    let forced_d0 = address.force_power_state_d0();
    crate::println!("hda: forced_d0={forced_d0}");
    if forced_d0 {
        let deadline = interrupts::ticks().wrapping_add(RESET_SETTLE_TICKS);
        while interrupts::ticks() < deadline {}
    }
    address.enable_memory_and_bus_master();
    let Some(mmio_base) = address.memory_bar(0) else {
        return Err(Status::InitFailed);
    };
    if paging::map_mmio(mmio_base, MMIO_BYTES).is_err() {
        return Err(Status::InitFailed);
    }
    crate::println!(
        "hda: mmio_base={:#x} gcap_pre={:#x} gctl_pre={:#x}",
        mmio_base,
        read_u16(mmio_base, REG_GCAP),
        read_u32(mmio_base, REG_GCTL)
    );
    let irq = address.interrupt_line();
    // Try MSI first; only fall back to the legacy `irq` line above if this
    // controller has no MSI capability at all, or every MSI vector is
    // already claimed. `enable_msi` performs no config-space writes at
    // all if it returns `None`, so there's nothing to undo on that path.
    let msi_vector = if crate::lapic::is_available() {
        interrupts::allocate_msi_vector()
            .filter(|&vector| address.enable_msi(vector, crate::lapic::id()).is_some())
    } else {
        None
    };

    if reset_controller(mmio_base).is_err() {
        return Err(Status::InitFailed);
    }

    let gcap = read_u16(mmio_base, REG_GCAP) as u32;
    let input_stream_count = ((gcap >> GCAP_ISS_SHIFT) & GCAP_ISS_MASK) as u8;
    let output_stream_count = ((gcap >> GCAP_OSS_SHIFT) & GCAP_OSS_MASK) as u8;

    let Some((corb_base, rirb_base)) = setup_corb_rirb(mmio_base) else {
        return Err(Status::InitFailed);
    };

    let mut hda = Hda {
        instance,
        mmio_base,
        irq,
        msi_vector,
        corb_base,
        rirb_base,
        rirb_rp: Cell::new(0),
        input_stream_count,
        output_stream_count,
        codecs: [None; MAX_CODECS],
        codec_count: 0,
        stream_index: None,
        playback_channels: 2,
        sample_rate: 44_100,
        bits_per_sample: BitDepth::Bits16,
        buffer_addr: 0,
        interrupt_count: AtomicU32::new(0),
        periods_serviced: AtomicU32::new(0),
        network_mode: AtomicBool::new(false),
        underrun_count: AtomicU32::new(0),
        capture_stream_index: None,
        capture_channels: 2,
        capture_buffer_addr: 0,
        capture_periods_serviced: AtomicU32::new(0),
        capture_overrun_count: AtomicU32::new(0),
    };
    discover_codecs(&mut hda);
    setup_playback(&mut hda);
    setup_capture(&mut hda);
    Ok(hda)
}

/// Locates a usable DAC (converter)/pin-complex pair to drive for
/// playback: for each discovered codec, prefers a pin that is both
/// output-capable and already connected (per its `GET_CONNECT_LIST`
/// response) to a widget this driver can confirm is an
/// `AudioOutput` converter. Falls back to simply pairing the first
/// `AudioOutput` widget with the first output-capable pin found on the
/// same codec if no confirmed-connected pair exists -- QEMU's synthetic
/// `hda-output`/`hda-duplex` codecs always report a real connection, so
/// this fallback exists purely for a real codec this driver hasn't been
/// tested against yet, not as the expected common case.
///
/// The returned 4th element is the negotiated channel ceiling for this
/// pair -- `min(dac.max_channels, pin.max_channels)`, per
/// `widget_max_channels`'s own doc comment -- so `setup_playback` never
/// needs to re-look-up either widget just to read its capability. The
/// 5th element is the DAC's own raw `AC_PAR_PCM` capability, passed
/// through unexamined for `setup_playback`'s `negotiate_format` call --
/// only the DAC's rate/bit-depth support matters here, not the pin's.
fn find_playback_target(hda: &Hda) -> Option<(u8, u8, u8, u8, u32)> {
    for codec in hda.codecs().iter().flatten() {
        let widgets = codec.widgets();
        for pin in widgets.iter().flatten() {
            if pin.widget_type != WidgetType::Pin || !pin.pin_output_capable {
                continue;
            }
            if let Some(source_nid) = pin.connection {
                let dac = widgets.iter().flatten().find(|w| {
                    w.nid == source_nid && w.widget_type == WidgetType::AudioOutput
                });
                if let Some(dac) = dac {
                    let max_channels = dac.max_channels.min(pin.max_channels);
                    return Some((codec.address, source_nid, pin.nid, max_channels, dac.pcm_formats));
                }
            }
        }

        let dac = widgets
            .iter()
            .flatten()
            .find(|w| w.widget_type == WidgetType::AudioOutput);
        let pin_widget = widgets
            .iter()
            .flatten()
            .find(|w| w.widget_type == WidgetType::Pin && w.pin_output_capable);
        if let (Some(dac), Some(pin_widget)) = (dac, pin_widget) {
            let max_channels = dac.max_channels.min(pin_widget.max_channels);
            return Some((codec.address, dac.nid, pin_widget.nid, max_channels, dac.pcm_formats));
        }
    }
    None
}

/// Locates a usable ADC (converter)/pin-complex pair to record from --
/// the capture-path mirror of `find_playback_target`. Per this module's
/// own doc comment on QEMU's synthetic `hda-duplex` codec layout, the
/// *pin*'s own connection entry names the ADC feeding it (the same
/// direction `find_playback_target` already reads for an output pin's
/// DAC source), so this reuses the identical two-pass strategy: prefer a
/// pin that is input-capable and already connected to a confirmed
/// `AudioInput` converter, falling back to simply pairing the first
/// `AudioInput` widget with the first input-capable pin on the same
/// codec. The returned 4th element mirrors `find_playback_target`'s own.
fn find_capture_target(hda: &Hda) -> Option<(u8, u8, u8, u8)> {
    for codec in hda.codecs().iter().flatten() {
        let widgets = codec.widgets();
        for pin in widgets.iter().flatten() {
            if pin.widget_type != WidgetType::Pin || !pin.pin_input_capable {
                continue;
            }
            if let Some(source_nid) = pin.connection {
                let adc = widgets.iter().flatten().find(|w| {
                    w.nid == source_nid && w.widget_type == WidgetType::AudioInput
                });
                if let Some(adc) = adc {
                    let max_channels = adc.max_channels.min(pin.max_channels);
                    return Some((codec.address, source_nid, pin.nid, max_channels));
                }
            }
        }

        let adc = widgets
            .iter()
            .flatten()
            .find(|w| w.widget_type == WidgetType::AudioInput);
        let pin_widget = widgets
            .iter()
            .flatten()
            .find(|w| w.widget_type == WidgetType::Pin && w.pin_input_capable);
        if let (Some(adc), Some(pin_widget)) = (adc, pin_widget) {
            let max_channels = adc.max_channels.min(pin_widget.max_channels);
            return Some((codec.address, adc.nid, pin_widget.nid, max_channels));
        }
    }
    None
}

/// Writes one 16-byte Buffer Descriptor List entry -- see `BDL_ENTRY_BYTES`'s
/// own doc comment for the field layout.
fn write_bdl_entry(bdl_base: u32, index: u32, addr: u64, len: u32, flags: u32) {
    let entry = bdl_base + index * BDL_ENTRY_BYTES;
    unsafe {
        write_volatile(entry as *mut u64, addr);
        write_volatile((entry + 8) as *mut u32, len);
        write_volatile((entry + 12) as *mut u32, flags);
    }
}

/// Brings up one playback stream on an already-reset, already-codec-
/// enumerated controller: allocates its DMA buffer and BDL, points a
/// discovered DAC/pin pair at a fixed stream tag/format, programs the
/// controller's own per-stream registers, and starts the stream running.
/// Leaves `hda.stream_index` at `None` (this controller simply has no
/// playback -- `Hda::has_playback` reports `false`, and `audio.rs`'s
/// facade skips it) on any failure along the way: a controller with no
/// usable codec, no free output stream, or a DMA allocation failure is
/// still perfectly usable for whatever a future step needs the CORB/RIRB
/// transport for, just not for playback.
fn setup_playback(hda: &mut Hda) {
    if hda.output_stream_count == 0 {
        return;
    }
    let Some((cad, dac_nid, pin_nid, negotiated_channels, pcm_formats)) = find_playback_target(hda)
    else {
        return;
    };
    // Clamp to this driver's own ceiling (`MAX_NEGOTIATED_CHANNELS`) and
    // never below 1 -- `widget_max_channels` already returns a value
    // >= 1, but a defensive `.max(1)` costs nothing and keeps this
    // function correct even against a widget reporting something
    // unexpected.
    let channels = negotiated_channels.clamp(1, MAX_NEGOTIATED_CHANNELS);
    let (base_44100, bits) = negotiate_format(pcm_formats);
    let format = stream_format(base_44100, bits, channels);
    let bytes_per_frame = channels as u32 * bits.bytes_per_sample();
    let period_bytes = PLAYBACK_PERIOD_FRAMES * bytes_per_frame;
    let buffer_bytes = PLAYBACK_TOTAL_FRAMES * bytes_per_frame;

    let buffer_frames = (buffer_bytes as usize).div_ceil(pmem::FRAME_SIZE as usize);
    let Some(buffer_addr) = pmem::allocate_contiguous(buffer_frames) else {
        return;
    };
    let Some(bdl_addr) = pmem::allocate() else {
        return;
    };
    unsafe {
        core::ptr::write_bytes(buffer_addr as *mut u8, 0, buffer_bytes as usize);
    }
    for half in 0..PLAYBACK_PERIOD_COUNT {
        let addr = (buffer_addr as u64) + (half * period_bytes) as u64;
        write_bdl_entry(bdl_addr, half, addr, period_bytes, BDL_FLAG_IOC);
    }

    // First output stream index -- input streams occupy the low indices
    // (see `STREAM_BASE`'s own doc comment).
    let stream_index = hda.input_stream_count;
    hda.stream_index = Some(stream_index);
    hda.playback_channels = channels;
    hda.sample_rate = if base_44100 { 44_100 } else { 48_000 };
    hda.bits_per_sample = bits;
    hda.buffer_addr = buffer_addr;

    let stream_off = stream_offset(stream_index);

    // Two-phase stream reset, the same bounded readback-polling
    // handshake `reset_controller` already uses for `GCTL_CRST`.
    hda.write_sd_ctl(stream_index, SD_CTL_STREAM_RESET);
    if !poll_until(|| hda.read_sd_ctl(stream_index) & SD_CTL_STREAM_RESET != 0) {
        hda.stream_index = None;
        return;
    }
    hda.write_sd_ctl(stream_index, 0);
    if !poll_until(|| hda.read_sd_ctl(stream_index) & SD_CTL_STREAM_RESET == 0) {
        hda.stream_index = None;
        return;
    }

    write_u32(hda.mmio_base, stream_off + SD_CBL, buffer_bytes);
    write_u16(hda.mmio_base, stream_off + SD_LVI, (PLAYBACK_PERIOD_COUNT - 1) as u16);
    write_u16(hda.mmio_base, stream_off + SD_FORMAT, format as u16);
    write_u32(hda.mmio_base, stream_off + SD_BDLPL, bdl_addr);
    write_u32(hda.mmio_base, stream_off + SD_BDLPU, 0);

    // Codec-side bring-up: format, stream/channel assignment, unmuted
    // output amp, and pin output enable. A codec that doesn't answer one
    // of these (or has no amplifier on this widget at all, in which case
    // the verb is simply ignored) is not treated as fatal -- the stream
    // is started regardless, exactly as generously as this driver
    // already treats an unanswered verb during discovery.
    hda.send_corb_command(verb16(cad, dac_nid, AC_VERB_SET_STREAM_FORMAT, format));
    hda.send_corb_command(verb(
        cad,
        dac_nid,
        AC_VERB_SET_CHANNEL_STREAMID,
        (PLAYBACK_STREAM_TAG << 4) & 0xf0,
    ));
    // Only sent when negotiation actually found more than the base
    // stereo pair -- a widget reporting `chan_cnt_ext = 0` (every QEMU
    // synthetic codec this driver has been tested against) implies
    // `channels <= 2`, where this verb is both unnecessary and not
    // guaranteed to be understood; see `AC_VERB_SET_CVT_CHAN_COUNT`'s own
    // doc comment.
    if channels > 2 {
        hda.send_corb_command(verb(
            cad,
            dac_nid,
            AC_VERB_SET_CVT_CHAN_COUNT,
            (channels - 1) as u32,
        ));
    }
    hda.send_corb_command(verb16(
        cad,
        dac_nid,
        AC_VERB_SET_AMP_GAIN_MUTE,
        AC_AMP_SET_OUTPUT | AC_AMP_SET_LEFT | AC_AMP_SET_RIGHT | AC_AMP_GAIN_DEFAULT,
    ));
    hda.send_corb_command(verb(cad, pin_nid, AC_VERB_SET_PIN_WIDGET_CONTROL, AC_PINCTL_OUT_EN));

    hda.fill_period(0);
    hda.fill_period(1);

    if let Some(vector) = hda.msi_vector {
        interrupts::register_msi_irq(vector, handle_pci_irq);
    } else if let Some(irq) = hda.irq {
        interrupts::register_pci_irq(irq, handle_pci_irq);
    }

    hda.write_sd_ctl(
        stream_index,
        (PLAYBACK_STREAM_TAG << SD_CTL_STREAM_TAG_SHIFT) | SD_CTL_IOCE | SD_CTL_RUN,
    );

    let intctl = read_u32(hda.mmio_base, REG_INTCTL);
    write_u32(
        hda.mmio_base,
        REG_INTCTL,
        intctl | INTCTL_GIE | INTCTL_CIE | (1 << stream_index),
    );
}

/// Brings up one capture (ADC) stream -- the input-path mirror of
/// `setup_playback`, differing only in: using the low input-stream
/// indices directly (stream index `0`, the first input stream, rather
/// than `output_stream_count`'s offset into the output range -- see
/// `STREAM_BASE`'s own doc comment on why input streams occupy the low
/// indices), a separate DMA buffer (`capture_buffer_addr`, never shared
/// with `buffer_addr`), `AC_AMP_SET_INPUT`/`AC_PINCTL_IN_EN` instead of
/// their output-path counterparts, and seeding the ring with nothing
/// (there is no silence-equivalent to pre-fill on the capture side --
/// the codec's own DMA simply hasn't written anything yet at this
/// point, exactly like `Es1370::start_adc` needs no analogous
/// `fill_period` priming call either). Leaves `hda.capture_stream_index`
/// at `None` (this controller simply has no capture -- `has_capture`
/// reports `false`) on any failure, exactly as generously as
/// `setup_playback` treats its own failure paths.
fn setup_capture(hda: &mut Hda) {
    if hda.input_stream_count == 0 {
        return;
    }
    let Some((cad, adc_nid, pin_nid, negotiated_channels)) = find_capture_target(hda) else {
        return;
    };
    let channels = negotiated_channels.clamp(1, MAX_NEGOTIATED_CHANNELS);
    // Reuses whatever `setup_playback` already negotiated (or the
    // driver's 44.1 kHz/16-bit default, if it found no playback target)
    // rather than negotiating independently against the ADC's own
    // `AC_PAR_PCM` -- the same shared-format convention `format()`
    // already documents for channel count, just extended to rate/bit
    // depth now that those vary too. `setup_playback` always runs first
    // (see `init_controller`), so `hda.sample_rate`/`bits_per_sample`
    // are already final by the time this reads them.
    let bits = hda.bits_per_sample;
    let base_44100 = hda.sample_rate != 48_000;
    let format = stream_format(base_44100, bits, channels);
    let bytes_per_frame = channels as u32 * bits.bytes_per_sample();
    let period_bytes = PLAYBACK_PERIOD_FRAMES * bytes_per_frame;
    let buffer_bytes = PLAYBACK_TOTAL_FRAMES * bytes_per_frame;

    let buffer_frames = (buffer_bytes as usize).div_ceil(pmem::FRAME_SIZE as usize);
    let Some(buffer_addr) = pmem::allocate_contiguous(buffer_frames) else {
        return;
    };
    let Some(bdl_addr) = pmem::allocate() else {
        return;
    };
    unsafe {
        core::ptr::write_bytes(buffer_addr as *mut u8, 0, buffer_bytes as usize);
    }
    for half in 0..PLAYBACK_PERIOD_COUNT {
        let addr = (buffer_addr as u64) + (half * period_bytes) as u64;
        write_bdl_entry(bdl_addr, half, addr, period_bytes, BDL_FLAG_IOC);
    }

    // First input stream index (see `STREAM_BASE`'s own doc comment on
    // why input streams occupy the low indices, starting at 0, unlike
    // `setup_playback`'s `input_stream_count`-offset output index).
    let stream_index: u8 = 0;
    hda.capture_stream_index = Some(stream_index);
    hda.capture_channels = channels;
    hda.capture_buffer_addr = buffer_addr;

    let stream_off = stream_offset(stream_index);

    hda.write_sd_ctl(stream_index, SD_CTL_STREAM_RESET);
    if !poll_until(|| hda.read_sd_ctl(stream_index) & SD_CTL_STREAM_RESET != 0) {
        hda.capture_stream_index = None;
        return;
    }
    hda.write_sd_ctl(stream_index, 0);
    if !poll_until(|| hda.read_sd_ctl(stream_index) & SD_CTL_STREAM_RESET == 0) {
        hda.capture_stream_index = None;
        return;
    }

    write_u32(hda.mmio_base, stream_off + SD_CBL, buffer_bytes);
    write_u16(hda.mmio_base, stream_off + SD_LVI, (PLAYBACK_PERIOD_COUNT - 1) as u16);
    write_u16(hda.mmio_base, stream_off + SD_FORMAT, format as u16);
    write_u32(hda.mmio_base, stream_off + SD_BDLPL, bdl_addr);
    write_u32(hda.mmio_base, stream_off + SD_BDLPU, 0);

    hda.send_corb_command(verb16(cad, adc_nid, AC_VERB_SET_STREAM_FORMAT, format));
    hda.send_corb_command(verb(
        cad,
        adc_nid,
        AC_VERB_SET_CHANNEL_STREAMID,
        (CAPTURE_STREAM_TAG << 4) & 0xf0,
    ));
    if channels > 2 {
        hda.send_corb_command(verb(
            cad,
            adc_nid,
            AC_VERB_SET_CVT_CHAN_COUNT,
            (channels - 1) as u32,
        ));
    }
    hda.send_corb_command(verb16(
        cad,
        adc_nid,
        AC_VERB_SET_AMP_GAIN_MUTE,
        AC_AMP_SET_INPUT | AC_AMP_SET_LEFT | AC_AMP_SET_RIGHT | AC_AMP_GAIN_DEFAULT,
    ));
    hda.send_corb_command(verb(cad, pin_nid, AC_VERB_SET_PIN_WIDGET_CONTROL, AC_PINCTL_IN_EN));

    if let Some(vector) = hda.msi_vector {
        interrupts::register_msi_irq(vector, handle_pci_irq);
    } else if let Some(irq) = hda.irq {
        interrupts::register_pci_irq(irq, handle_pci_irq);
    }

    hda.write_sd_ctl(
        stream_index,
        (CAPTURE_STREAM_TAG << SD_CTL_STREAM_TAG_SHIFT) | SD_CTL_IOCE | SD_CTL_RUN,
    );

    let intctl = read_u32(hda.mmio_base, REG_INTCTL);
    write_u32(
        hda.mmio_base,
        REG_INTCTL,
        intctl | INTCTL_GIE | INTCTL_CIE | (1 << stream_index),
    );
}

/// Returns how many controllers `initialize_all` discovered (0 if it was
/// never called, or found none).
pub fn count() -> usize {
    unsafe { *core::ptr::addr_of!(CONTROLLER_COUNT) }
}

/// Returns the `index`-th (0-based) discovered controller, or `None` if
/// out of range. Assumes `initialize_all` already ran.
pub fn get(index: usize) -> Option<&'static Hda> {
    unsafe { (*core::ptr::addr_of!(CONTROLLERS)).get(index)?.as_ref() }
}

/// Clears then sets `GCTL_CRST`, polling (bounded) for the bit to read
/// back each way before proceeding -- real hardware needs the readback
/// polling even though QEMU's own emulation applies it instantaneously
/// (see `intel_hda_set_g_ctl`'s unconditional `device_cold_reset` call).
///
/// **Found and fixed against real hardware** (a 6-series/Cougar Point
/// PCH's onboard HDA controller, `DEVICE_CPT`): entering and exiting
/// reset back-to-back with no settle delay between them -- fine under
/// QEMU's instantaneous emulation -- left real silicon failing to
/// complete the transition, so the second poll below always timed out.
/// Cross-checked against Linux's own `snd_hdac_bus_reset_link`
/// (`sound/hda/core/controller.c`): the HDA spec (Rev 0.9, section
/// 5.5.1) requires "delay for >= 100us for codec PLL to settle" between
/// asserting and deasserting the controller reset. `RESET_SETTLE_TICKS`
/// (one 100 Hz PIT tick, ~10ms) is far more generous than that minimum,
/// the same margin-over-spec convention `CODEC_ENUMERATION_WAIT_TICKS`
/// below already uses for the analogous post-reset codec-init wait.
fn reset_controller(mmio_base: u32) -> Result<(), Status> {
    let gctl = read_u32(mmio_base, REG_GCTL);
    write_u32(mmio_base, REG_GCTL, gctl & !GCTL_CRST);
    if !poll_until(|| read_u32(mmio_base, REG_GCTL) & GCTL_CRST == 0) {
        return Err(Status::InitFailed);
    }

    let settle_deadline = interrupts::ticks().wrapping_add(RESET_SETTLE_TICKS);
    while interrupts::ticks() < settle_deadline {}

    write_u32(mmio_base, REG_GCTL, gctl | GCTL_CRST);
    if !poll_until(|| read_u32(mmio_base, REG_GCTL) & GCTL_CRST != 0) {
        return Err(Status::InitFailed);
    }

    let deadline = interrupts::ticks().wrapping_add(CODEC_ENUMERATION_WAIT_TICKS);
    while interrupts::ticks() < deadline {}
    Ok(())
}

fn poll_until(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..POLL_ATTEMPTS {
        if condition() {
            return true;
        }
    }
    false
}

/// Allocates one physical frame each for the CORB (256 * 4 = 1024
/// bytes) and RIRB (256 * 8 = 2048 bytes) rings -- both comfortably fit
/// in a single 4 KiB `pmem` frame apiece, the same "one general-pool
/// frame is plenty for this fixed-size DMA structure" pattern already
/// used elsewhere in this project. Zero-fills both before programming
/// their base-address registers, so stale frame contents already in
/// memory are never misread as a real verb/response; stops both DMA
/// engines first, since the base-address registers are only meant to be
/// reprogrammed while their engines are idle.
fn setup_corb_rirb(mmio_base: u32) -> Option<(u32, u32)> {
    write_u8(mmio_base, REG_CORBCTL, 0);
    write_u8(mmio_base, REG_RIRBCTL, 0);

    let corb_base = pmem::allocate()?;
    let rirb_base = pmem::allocate()?;
    unsafe {
        core::ptr::write_bytes(corb_base as *mut u8, 0, pmem::FRAME_SIZE as usize);
        core::ptr::write_bytes(rirb_base as *mut u8, 0, pmem::FRAME_SIZE as usize);
    }

    write_u32(mmio_base, REG_CORBLBASE, corb_base);
    write_u32(mmio_base, REG_CORBUBASE, 0);
    write_u32(mmio_base, REG_RIRBLBASE, rirb_base);
    write_u32(mmio_base, REG_RIRBUBASE, 0);

    // Confirm the ring size both registers actually selected (see this
    // module's doc comment on why this driver reads it back rather than
    // attempting to write a particular selection); a controller
    // reporting anything other than the expected 256-entry size is
    // treated as unsupported rather than silently mis-addressing the
    // ring with the wrong stride.
    let corb_size = read_u8(mmio_base, REG_CORBSIZE);
    let rirb_size = read_u8(mmio_base, REG_RIRBSIZE);
    if (corb_size & 0x03) != 0x02 || (rirb_size & 0x03) != 0x02 {
        return None;
    }

    // Two-phase CORB read-pointer reset handshake -- see `CORBRP_RST`'s
    // own doc comment.
    write_u16(mmio_base, REG_CORBRP, CORBRP_RST);
    if !poll_until(|| read_u16(mmio_base, REG_CORBRP) & CORBRP_RST != 0) {
        return None;
    }
    write_u16(mmio_base, REG_CORBRP, 0);
    if !poll_until(|| read_u16(mmio_base, REG_CORBRP) & CORBRP_RST == 0) {
        return None;
    }
    write_u16(mmio_base, REG_CORBWP, 0);

    // RIRB write-pointer reset is a self-clearing pulse -- see
    // `RIRBWP_RST`'s own doc comment.
    write_u16(mmio_base, REG_RIRBWP, RIRBWP_RST);
    write_u16(mmio_base, REG_RINTCNT, RINTCNT_MAX);

    write_u8(mmio_base, REG_CORBCTL, CORBCTL_RUN);
    write_u8(mmio_base, REG_RIRBCTL, RIRBCTL_DMA_EN);

    Some((corb_base, rirb_base))
}

/// Reads `STATESTS` and, for each present codec address, walks its node
/// graph: root -> Audio Function Group -> AFG's own child widgets.
fn discover_codecs(hda: &mut Hda) {
    let statests = read_u16(hda.mmio_base, REG_STATESTS) & STATESTS_MASK;
    for cad in 0..MAX_CODEC_ADDRESS {
        if statests & (1 << cad) == 0 {
            continue;
        }
        let cad = cad as u8;
        let Some(response) =
            hda.send_corb_command(verb(cad, AC_NODE_ROOT as u8, AC_VERB_PARAMETERS, AC_PAR_VENDOR_ID))
        else {
            continue;
        };
        let vendor_id = (response >> 16) as u16;
        let device_id = response as u16;

        let afg_nid = find_audio_function_group(hda, cad);
        let (widgets, widget_count) = match afg_nid {
            Some(afg_nid) => enumerate_afg_widgets(hda, cad, afg_nid),
            None => ([None; MAX_WIDGETS], 0),
        };

        if hda.codec_count < MAX_CODECS {
            hda.codecs[hda.codec_count] = Some(CodecInfo {
                address: cad,
                vendor_id,
                device_id,
                afg_nid,
                widgets,
                widget_count,
            });
            hda.codec_count += 1;
        }
    }
}

/// Computes a widget's maximum supported channel count from its raw
/// `AUDIO_WIDGET_CAP` value -- see `AC_WCAP_CHAN_CNT_EXT_SHIFT`'s own doc
/// comment for the formula and its Linux-kernel citation.
fn widget_max_channels(cap: u32) -> u8 {
    if cap & AC_WCAP_STEREO == 0 {
        1
    } else {
        let ext = (cap >> AC_WCAP_CHAN_CNT_EXT_SHIFT) & AC_WCAP_CHAN_CNT_EXT_MASK;
        (((ext << 1) | 1) + 1) as u8
    }
}

/// Reads the root node's `NODE_COUNT` to find its subordinate nodes,
/// then reads each one's `FUNCTION_TYPE` looking for
/// `AC_GRP_AUDIO_FUNCTION`. Real codecs may expose more than one
/// function group (e.g. audio plus modem); this returns the first
/// audio one found, which is all a single-AFG codec (every one this
/// project has tested against) has anyway.
fn find_audio_function_group(hda: &Hda, cad: u8) -> Option<u8> {
    let node_count = hda.send_corb_command(verb(
        cad,
        AC_NODE_ROOT as u8,
        AC_VERB_PARAMETERS,
        AC_PAR_NODE_COUNT,
    ))?;
    let start_nid = ((node_count >> 16) & 0xff) as u8;
    let count = (node_count & 0xff) as u32;

    for i in 0..count {
        let nid = start_nid.wrapping_add(i as u8);
        let Some(function_type) =
            hda.send_corb_command(verb(cad, nid, AC_VERB_PARAMETERS, AC_PAR_FUNCTION_TYPE))
        else {
            continue;
        };
        if (function_type & 0xff) == AC_GRP_AUDIO_FUNCTION {
            return Some(nid);
        }
    }
    None
}

/// Reads the AFG's `NODE_COUNT` to find its child-widget range, then for
/// each widget reads `AUDIO_WIDGET_CAP` and, depending on the widget's
/// own reported capabilities, `PIN_CAP` and/or `CONNLIST_LEN` +
/// `GET_CONNECT_LIST`.
fn enumerate_afg_widgets(
    hda: &Hda,
    cad: u8,
    afg_nid: u8,
) -> ([Option<WidgetInfo>; MAX_WIDGETS], usize) {
    let mut widgets: [Option<WidgetInfo>; MAX_WIDGETS] = [None; MAX_WIDGETS];
    let mut count = 0;

    let Some(node_count) =
        hda.send_corb_command(verb(cad, afg_nid, AC_VERB_PARAMETERS, AC_PAR_NODE_COUNT))
    else {
        return (widgets, 0);
    };
    let start_nid = ((node_count >> 16) & 0xff) as u8;
    let widget_total = (node_count & 0xff) as usize;

    for i in 0..widget_total.min(MAX_WIDGETS) {
        let nid = start_nid.wrapping_add(i as u8);
        let Some(cap) =
            hda.send_corb_command(verb(cad, nid, AC_VERB_PARAMETERS, AC_PAR_AUDIO_WIDGET_CAP))
        else {
            continue;
        };
        let widget_type = WidgetType::from_raw(((cap >> AC_WCAP_TYPE_SHIFT) & AC_WCAP_TYPE_MASK) as u8);
        let stereo = cap & AC_WCAP_STEREO != 0;
        let max_channels = widget_max_channels(cap);

        let mut pin_output_capable = false;
        let mut pin_input_capable = false;
        if widget_type == WidgetType::Pin {
            if let Some(pin_cap) =
                hda.send_corb_command(verb(cad, nid, AC_VERB_PARAMETERS, AC_PAR_PIN_CAP))
            {
                pin_output_capable = pin_cap & AC_PINCAP_OUT != 0;
                pin_input_capable = pin_cap & AC_PINCAP_IN != 0;
            }
        }

        let mut connection = None;
        if cap & AC_WCAP_CONN_LIST != 0 {
            if let Some(clist_len) =
                hda.send_corb_command(verb(cad, nid, AC_VERB_PARAMETERS, AC_PAR_CONNLIST_LEN))
            {
                if (clist_len & AC_CLIST_LENGTH_MASK) != 0 {
                    if let Some(list) =
                        hda.send_corb_command(verb(cad, nid, AC_VERB_GET_CONNECT_LIST, 0))
                    {
                        connection = Some((list & 0xff) as u8);
                    }
                }
            }
        }

        // Only a converter widget's own PCM capability matters for
        // format negotiation (see `negotiate_format`); querying it for
        // every other widget type would just waste CORB round trips.
        let pcm_formats = if widget_type == WidgetType::AudioOutput
            || widget_type == WidgetType::AudioInput
        {
            hda.send_corb_command(verb(cad, nid, AC_VERB_PARAMETERS, AC_PAR_PCM))
                .unwrap_or(0)
        } else {
            0
        };

        if count < MAX_WIDGETS {
            widgets[count] = Some(WidgetInfo {
                nid,
                widget_type,
                stereo,
                max_channels,
                pin_output_capable,
                pin_input_capable,
                pcm_formats,
                connection,
            });
            count += 1;
        }
    }

    (widgets, count)
}
