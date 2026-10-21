//! USB Audio Class devices: recognise them and report what they offer.
//!
//! An audio device's configuration holds one *AudioControl* interface (class 1, subclass 1:
//! the topology, and for Audio Class 2.0 the clock sources) and one or more *AudioStreaming*
//! interfaces (subclass 2). A streaming interface has an *alternate setting 0* with no
//! endpoints (the idle state) and further alternate settings, each describing one format
//! (channels, bit resolution) and carrying the isochronous endpoint audio flows over; the host
//! selects an alternate setting to start streaming. Class 1.0 (what the old TerraTec card used)
//! lists its sample rates in the format descriptor; class 2.0 (most modern interfaces) asks the
//! *clock source* entity with a `GET RANGE` request instead.
//!
//! This module only parses and prints, so the first look at a new device shows exactly what it
//! presents before any streaming code exists.

#[derive(Clone, Copy, Default)]
pub struct StreamAlt {
    pub interface: u8,
    pub alt: u8,
    pub channels: u8,
    /// Bytes per sample slot in the isochronous packets, and how many of its bits carry audio.
    pub subslot_bytes: u8,
    pub bits: u8,
    /// The samples are IEEE 754 single-precision floats (a 4-byte slot), not integers: UAC1 wFormatTag 3, UAC2 bmFormats bit 2.
    pub float: bool,
    pub endpoint: u8,
    /// Bits 3:2 of the endpoint's attributes: 0 none, 1 asynchronous, 2 adaptive, 3 synchronous.
    pub sync: u8,
    pub max_packet: u16,
    /// High-bandwidth endpoints (high speed): extra transactions per microframe, 0 to 2 (wMaxPacketSize bits 12:11).
    pub extra_transactions: u8,
    pub interval: u8,
    /// The explicit feedback endpoint of an asynchronous OUT stream, if any.
    pub feedback: u8,
    pub feedback_packet: u16,
    pub feedback_interval: u8,
    /// Class 1.0 lists its discrete sample rates in the format descriptor (0 rates = continuous range).
    pub rates: [u32; 6],
    pub rate_count: usize,
    pub continuous_range: (u32, u32),
}

pub const MAX_ALTS: usize = 40;

pub struct Description {
    /// 1 or 2 (Audio Class 1.0 / 2.0), 0 if the control interface said nothing.
    pub class_version: u8,
    pub control_interface: u8,
    pub clock_sources: [u8; 4],
    pub clock_count: usize,
    pub alts: [StreamAlt; MAX_ALTS],
    pub alt_count: usize,
}

/// True if the configuration has an AudioStreaming interface.
pub fn is_audio(configuration: &[u8]) -> bool {
    let mut at = 0;
    while at + 9 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            break;
        }
        if configuration[at + 1] == 4 && configuration[at + 5] == 1 && configuration[at + 6] == 2 {
            return true;
        }
        at += length;
    }
    false
}

/// Walks the configuration descriptor and collects every streaming alternate setting that has a
/// data endpoint.
pub fn parse(configuration: &[u8]) -> Description {
    let mut description = Description { class_version: 0, control_interface: 0, clock_sources: [0; 4], clock_count: 0, alts: [StreamAlt::default(); MAX_ALTS], alt_count: 0 };
    let (mut class, mut subclass) = (0u8, 0u8);
    let mut current = StreamAlt::default();
    let mut uac2_stream = false;
    let mut at = 0;
    let flush = |description: &mut Description, current: &StreamAlt| {
        if current.endpoint != 0 && description.alt_count < MAX_ALTS {
            description.alts[description.alt_count] = *current;
            description.alt_count += 1;
        }
    };
    while at + 2 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            break;
        }
        let d = &configuration[at..at + length];
        match d[1] {
            4 if length >= 9 => {
                if subclass == 2 {
                    flush(&mut description, &current);
                }
                class = d[5];
                subclass = d[6];
                uac2_stream = d[7] == 0x20;
                current = StreamAlt { interface: d[2], alt: d[3], ..StreamAlt::default() };
                if class == 1 && subclass == 1 {
                    description.control_interface = d[2];
                }
                if class == 1 && uac2_stream && description.class_version == 0 {
                    description.class_version = 2;
                }
            }
            0x24 if class == 1 && length >= 4 => match (subclass, d[2]) {
                (1, 1) if length >= 6 => description.class_version = if u16::from_le_bytes([d[3], d[4]]) >= 0x0200 { 2 } else { 1 },
                (1, 0x0a) if description.clock_count < description.clock_sources.len() => {
                    description.clock_sources[description.clock_count] = d[3];
                    description.clock_count += 1;
                }
                (2, 1) if uac2_stream && length >= 11 => {
                    // AS_GENERAL (class 2.0): bmFormats at 6..10, bit 2 is TYPE_I IEEE_FLOAT
                    current.channels = d[10];
                    current.float = d[6] & 4 != 0;
                }
                (2, 1) if !uac2_stream && length >= 7 => current.float = u16::from_le_bytes([d[5], d[6]]) == 3, // wFormatTag IEEE_FLOAT
                (2, 2) if uac2_stream && length >= 6 => {
                    current.subslot_bytes = d[4];
                    current.bits = d[5];
                }
                (2, 2) if !uac2_stream && length >= 8 => {
                    // FORMAT_TYPE_I (class 1.0): channels, subframe size, bit resolution
                    current.channels = d[4];
                    current.subslot_bytes = d[5];
                    current.bits = d[6];
                    let rate = |at: usize| u32::from(d[at]) | u32::from(d[at + 1]) << 8 | u32::from(d[at + 2]) << 16;
                    let kinds = usize::from(d[7]);
                    if kinds == 0 && length >= 14 {
                        current.continuous_range = (rate(8), rate(11)); // lower and upper limit
                    }
                    for index in 0..kinds.min(current.rates.len()) {
                        if length >= 8 + 3 * (index + 1) {
                            current.rates[index] = rate(8 + 3 * index);
                            current.rate_count = index + 1;
                        }
                    }
                }
                _ => {}
            },
            5 if subclass == 2 && length >= 7 => {
                let attributes = d[3];
                if attributes & 3 == 1 {
                    // isochronous: a data endpoint (usage 0 or 2) or an explicit feedback one (usage 1)
                    if (attributes >> 4) & 3 == 1 {
                        current.feedback = d[2];
                        current.feedback_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
                        current.feedback_interval = d[6];
                    } else if current.endpoint == 0 {
                        current.endpoint = d[2];
                        current.sync = (attributes >> 2) & 3;
                        let raw = u16::from_le_bytes([d[4], d[5]]);
                        current.max_packet = raw & 0x7ff;
                        current.extra_transactions = ((raw >> 11) & 3) as u8;
                        current.interval = d[6];
                    }
                }
            }
            _ => {}
        }
        at += length;
    }
    if subclass == 2 {
        flush(&mut description, &current);
    }
    description
}

fn sync_name(sync: u8) -> &'static str {
    match sync {
        0 => "no synchronisation",
        1 => "asynchronous",
        2 => "adaptive",
        _ => "synchronous",
    }
}

/// Prints what a device offers, one line per usable alternate setting.
pub fn report(description: &Description, path: &dyn core::fmt::Display, vendor: u16, product: u16, speed: &str) {
    crate::println!(
        "USB audio {vendor:04x}:{product:04x} on port {path} ({speed}): Audio Class {}, {} clock source(s), {} streaming alternate setting(s).",
        match description.class_version {
            2 => "2.0",
            1 => "1.0",
            _ => "(version not stated)",
        },
        description.clock_count,
        description.alt_count
    );
    for alt in &description.alts[..description.alt_count] {
        crate::println!(
            "  interface {} alt {}: {} {} ch, {}{}-bit in {}-byte slots, endpoint {:#04x} isochronous ({}), max packet {}{}, interval {}",
            alt.interface,
            alt.alt,
            if alt.endpoint & 0x80 != 0 { "capture" } else { "playback" },
            alt.channels,
            if alt.float { "float " } else { "" },
            alt.bits,
            alt.subslot_bytes,
            alt.endpoint,
            sync_name(alt.sync),
            alt.max_packet,
            match alt.extra_transactions {
                1 => " x2 per microframe",
                2 => " x3 per microframe",
                _ => "",
            },
            alt.interval
        );
        if alt.rate_count > 0 {
            crate::print(format_args!("    sample rates:"));
            for rate in &alt.rates[..alt.rate_count] {
                crate::print(format_args!(" {rate}"));
            }
            crate::println!(" Hz");
        } else if alt.continuous_range.1 != 0 {
            crate::println!("    sample rates: {} to {} Hz (continuous)", alt.continuous_range.0, alt.continuous_range.1);
        }
        if alt.feedback != 0 {
            crate::println!("    feedback endpoint {:#04x} (the device's real sample clock)", alt.feedback);
        }
    }
}

// ------------------------------------------------------------------------------------------
// Streaming
// ------------------------------------------------------------------------------------------
//
// A started stream keeps a *window* of isochronous transfers queued on its endpoint, one packet each
// (what the endpoint carries per service interval: 1 ms at full speed, 125 us at high speed). The
// controller completes them in order, one event each. Those events are handled **inline by whoever
// drains the xHCI event ring** (`xhci::drain`: the MSI interrupt handler, or the foreground with
// interrupts off when MSI is not in use) through `irq_event`: capture appends the received bytes to a
// PCM ring, playback fills the packet from a PCM ring (silence when it is empty, counted as an
// underrun), and the packet slot goes back to the end of the window. So audio keeps flowing while the
// foreground is busy with something long, such as a disk write; with MSI the foreground need not run at
// all. The window (~24 ms at high speed, ~190 ms at full speed) only has to outlast interrupt latency.
//
// Concurrency: `irq_event` runs in interrupt context, the shell and tapes in the foreground. They share
// only (a) the PCM rings, single-producer single-consumer with atomic head/tail, (b) counters, atomics,
// and (c) a stream object, which the foreground builds completely, publishes with interrupts off, and
// later unpublishes the same way after clearing its `active` flag. Everything else about a published
// stream (its position in the window, its transfer ring, the feedback state) belongs to `irq_event`.

use crate::ehci;
use crate::memory;
use crate::xhci::{self, Controller, Device, Ring as TransferRing};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// Most transfers kept queued per stream (the array size). Capture keeps all of them (24 ms at high speed: a
/// capture packet is delivered when it completes, so a long window adds no latency). Playback keeps only about
/// `PLAYBACK_WINDOW_MS` of them, because a packet is filled when it is queued and plays a whole window later:
/// the window *is* the playback latency.
const WINDOW: usize = 192;
const PLAYBACK_WINDOW_MS: u32 = 3;
/// PCM ring size in bytes (about a second of 6-channel 24-bit audio at 48 kHz fits).
pub(crate) const RING_BYTES: usize = 1 << 20;
/// The capture ring is bigger: a recording to a USB stick can stall for seconds while the stick erases flash, and
/// the ring is what rides that out (16 MiB is five seconds of 22 channels of 24-bit audio).
const CAPTURE_RING_BYTES: usize = 16 << 20;
/// The capture ring size a tape asked for (`audio_set_capture_ring`), 0 for the default.
static WANTED_CAPTURE_RING: AtomicUsize = AtomicUsize::new(0);

pub fn set_capture_ring(bytes: u64) {
    WANTED_CAPTURE_RING.store(bytes as usize, Ordering::Relaxed);
}

/// Exercises a ring made of three separate blocks of different sizes (so frames and chunks straddle block boundaries and
/// the wrap-around): pushes and pops odd-sized pieces of a counting pattern for many laps and checks every byte. Run at boot,
/// since the recorder's multi-gigabyte capture ring is built the same way and a mistake here would silently scramble audio.
pub fn ring_self_test() {
    let sizes = [8192usize, 12288, 4096];
    let mut ring = PcmRing::empty();
    for &size in &sizes {
        // A separate allocation each time, with a gap kept between them so they are not one block.
        let Some(base) = memory::allocate_contiguous((size / 4096) as u64, None) else {
            crate::println!("PCM ring self-test: no memory for the test blocks (skipped).");
            ring.free();
            return;
        };
        ring.push_segment(base, size);
    }
    let (mut next_in, mut next_out) = (0u32, 0u32);
    let mut buffer = [0u8; 5000];
    let mut ok = true;
    let mut step = 1usize;
    for round in 0..4000usize {
        // Push a piece of a size that varies, whole or not at all.
        step = (step * 7 + 13 + round) % 4999 + 1;
        for (index, byte) in buffer[..step].iter_mut().enumerate() {
            *byte = (next_in.wrapping_add(index as u32) % 251) as u8;
        }
        if ring.push_whole(&buffer[..step]) == step {
            next_in = next_in.wrapping_add(step as u32);
        }
        // Pop a different varying amount and check it is the next bytes in order.
        let want = (step * 3 + round) % 4000 + 1;
        let got = ring.pop(&mut buffer[..want]);
        for (index, &byte) in buffer[..got].iter().enumerate() {
            ok &= byte == (next_out.wrapping_add(index as u32) % 251) as u8;
        }
        next_out = next_out.wrapping_add(got as u32);
    }
    ok &= ring.available() as u32 == next_in.wrapping_sub(next_out);
    ring.free();
    crate::println!("PCM ring self-test ({} blocks, {} laps): {}.", sizes.len(), next_in as usize / (8192 + 12288 + 4096), if ok { "passed" } else { "FAILED" });
}

/// The capture ring: as much as was asked for, taken from as many blocks as it needs. A block is one contiguous run of
/// memory and the biggest run on a machine whose RAM is split by the PCI hole is a fraction of the free total, so the ring
/// is built from the biggest run, then the next biggest, until it is big enough or the memory (or `MAX_SEGMENTS`) is used
/// up. The first block is at least the default size, or there is no ring.
pub(crate) fn allocate_capture_ring() -> Option<PcmRing> {
    let wanted = WANTED_CAPTURE_RING.load(Ordering::Relaxed).max(CAPTURE_RING_BYTES);
    let mut ring = PcmRing::empty();
    while ring.size < wanted && ring.count < MAX_SEGMENTS {
        let run = memory::largest_free_run() as usize * memory::FRAME as usize;
        let here = (wanted - ring.size).min(run) & !4095;
        // Later blocks smaller than this are not worth a slot.
        if here < MIN_SEGMENT && !(ring.count == 0 && here > 0) {
            break;
        }
        // Not cleared: a ring is only read where it has been written, and clearing gigabytes delays the start of capture.
        match memory::allocate_contiguous_uncleared((here / 4096) as u64, None) {
            Some(base) => ring.push_segment(base, here),
            None => break,
        }
    }
    if ring.size < CAPTURE_RING_BYTES {
        ring.free();
        return None;
    }
    ring.check();
    Some(ring)
}
const FEEDBACK_WINDOW: usize = 8;

/// Most blocks a ring can be made of, and the smallest block worth adding after the first.
const MAX_SEGMENTS: usize = 32;
const MIN_SEGMENT: usize = 16 << 20;

/// A byte FIFO between the foreground and the interrupt handler (one producer, one consumer). Its memory is one or more
/// blocks (`segments`: physical address and length) that together make a ring of `size` bytes; a position in the FIFO is
/// found by walking them, and a copy that crosses from one block to the next is two copies.
pub struct PcmRing {
    segments: [(u64, usize); MAX_SEGMENTS],
    count: usize,
    pub(crate) size: usize,
    head: AtomicUsize, // next byte to write (producer)
    tail: AtomicUsize, // next byte to read (consumer)
    pub(crate) overruns: AtomicU64,
}

impl PcmRing {
    /// An empty ring, all zero (usable in a `static`, so it can live in `.bss`).
    pub(crate) const EMPTY: PcmRing = PcmRing { segments: [(0, 0); MAX_SEGMENTS], count: 0, size: 0, head: AtomicUsize::new(0), tail: AtomicUsize::new(0), overruns: AtomicU64::new(0) };

    pub(crate) fn empty() -> PcmRing {
        PcmRing::EMPTY
    }

    pub(crate) fn push_segment(&mut self, base: u64, length: usize) {
        self.segments[self.count] = (base, length);
        self.count += 1;
        self.size += length;
    }

    pub(crate) fn allocate(size: usize) -> Option<PcmRing> {
        let mut ring = PcmRing::empty();
        ring.push_segment(memory::allocate_contiguous((size / 4096) as u64, None)?, size);
        Some(ring)
    }

    /// The address of byte `position` of the FIFO (counted from its start, wrapping at `size`) and how many bytes follow it
    /// in the same block.
    fn locate(&self, position: usize) -> (usize, usize) {
        let mut offset = position % self.size;
        for &(base, length) in &self.segments[..self.count] {
            if offset < length {
                return (base as usize + offset, length - offset);
            }
            offset -= length;
        }
        (0, 0)
    }

    /// Proves each block of the ring's memory can be written and read back at both ends, and says where the ring is: a ring
    /// of gigabytes lies mostly above 4 GiB, which nothing in the kernel had used before.
    fn check(&self) {
        let mut ok = true;
        let mut highest = 0u64;
        for &(base, length) in &self.segments[..self.count] {
            for offset in [0, length - 8] {
                let at = (base as usize + offset) as *mut u64;
                unsafe {
                    at.write_volatile(0x5a5a_a5a5_0f0f_f0f0 ^ offset as u64);
                    ok &= at.read_volatile() == 0x5a5a_a5a5_0f0f_f0f0 ^ offset as u64;
                    at.write_volatile(0);
                }
            }
            highest = highest.max(base + length as u64);
        }
        if self.size > CAPTURE_RING_BYTES {
            crate::println!(
                "USB audio: capture ring of {} MiB in {} block(s), up to {:#x}{}: {}.",
                self.size >> 20,
                self.count,
                highest,
                if highest > 1 << 32 { " (above 4 GiB)" } else { "" },
                if ok { "memory checked" } else { "READ-BACK FAILED" }
            );
        }
    }

    pub(crate) fn available(&self) -> usize {
        self.head.load(Ordering::Acquire).wrapping_sub(self.tail.load(Ordering::Acquire))
    }
    pub(crate) fn space(&self) -> usize {
        self.size - self.available()
    }
    /// Appends the data if all of it fits; otherwise none of it is taken and the overrun is counted (the reader is too
    /// slow). Never part of it: a captured packet is whole frames, and a ring that took half a packet would leave every
    /// later frame shifted by a few bytes, scrambling the channels for the rest of a recording.
    pub(crate) fn push_whole(&self, data: &[u8]) -> usize {
        if data.len() <= self.space() {
            self.push(data)
        } else {
            self.overruns.fetch_add(1, Ordering::Relaxed);
            0
        }
    }

    /// Appends what fits; the rest is dropped and counted (the reader is too slow).
    pub(crate) fn push(&self, data: &[u8]) -> usize {
        let count = data.len().min(self.space());
        let head = self.head.load(Ordering::Relaxed);
        let mut done = 0;
        while done < count {
            let (address, run) = self.locate(head.wrapping_add(done));
            let chunk = run.min(count - done);
            unsafe { core::ptr::copy_nonoverlapping(data[done..].as_ptr(), address as *mut u8, chunk) };
            done += chunk;
        }
        self.head.store(head.wrapping_add(count), Ordering::Release);
        if count < data.len() {
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
        count
    }
    pub(crate) fn pop(&self, out: &mut [u8]) -> usize {
        let count = out.len().min(self.available());
        let tail = self.tail.load(Ordering::Relaxed);
        let mut done = 0;
        while done < count {
            let (address, run) = self.locate(tail.wrapping_add(done));
            let chunk = run.min(count - done);
            unsafe { core::ptr::copy_nonoverlapping(address as *const u8, out[done..].as_mut_ptr(), chunk) };
            done += chunk;
        }
        self.tail.store(tail.wrapping_add(count), Ordering::Release);
        count
    }
    pub(crate) fn free(&self) {
        for &(base, length) in &self.segments[..self.count] {
            if base != 0 {
                memory::free(base, (length / 4096) as u64);
            }
        }
    }
}

/// The explicit feedback endpoint of an asynchronous playback stream: the device says, every so often,
/// how many samples per frame its own clock consumes. Full speed sends it as 10.14 fixed point in 3 bytes
/// (samples per 1 ms frame), high speed as 16.16 in 4 bytes (samples per 125 us microframe).
struct Feedback {
    dci: u32,
    ring: TransferRing,
    doorbell: usize,
    buffers: u64,
    trbs: [u64; FEEDBACK_WINDOW],
    next: usize,
    packet: u16,
    /// The last accepted value, in Q16 samples per packet, and how many were received / rejected.
    last_q16: u32,
    /// The lowest and highest accepted values (Q16 samples per packet), reported when the stream stops.
    min_q16: u32,
    max_q16: u32,
    received: AtomicU64,
    rejected: AtomicU64,
}

struct Stream {
    alt: StreamAlt,
    dci: u32,
    capture: bool,
    /// xHCI only: the transfer ring (EHCI streams use `ehci` instead).
    ring: Option<TransferRing>,
    doorbell: usize,
    buffers: u64,
    frames: u64,
    ehci: Option<EhciPipe>,
    /// Slot whose completion is expected next, and the TRB address queued for each slot.
    next: usize,
    trbs: [u64; WINDOW],
    /// How many of `trbs` are in use (capture: all; playback: about 6 ms worth).
    window: usize,
    pcm: PcmRing,
    /// Playback only: frames per packet in Q16 fixed point (nominal, or what the device's feedback endpoint
    /// last reported) and the fractional remainder carried from packet to packet.
    step_q16: u32,
    accumulator_q16: u32,
    feedback: Option<Feedback>,
    packets_per_second: u32,
    rate: u32,
    /// Set (with interrupts off) when the stream is published, cleared before it is torn down.
    active: AtomicBool,
    packets: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    underruns: AtomicU64,
    /// A capture stream that only measures the device's clock for a playback stream without a feedback endpoint (EHCI):
    /// its data is counted (frames, packets) and dropped.
    monitor: bool,
    mon_frames: u64,
    mon_packets: u64,
}

/// An EHCI stream: a ring of `ehci::ISO_RING` iTDs, one per frame, each refilled after it completes (see `ehci_service`).
#[derive(Clone, Copy)]
struct EhciPipe {
    controller_index: usize,
    /// 0 = playback, 1 = capture: which of the controller's two iTD rings.
    which: usize,
    itds: u64,
    /// The iTD completed (or due) next.
    next: usize,
    /// Microframes between transactions (2^(bInterval-1)) and transactions per iTD (8 / step).
    step: usize,
    per_itd: usize,
    /// Bytes of buffer per iTD (a multiple of the page size, as the iTD's buffer pointers are page based).
    region: usize,
}

impl Stream {
    fn nominal_step(&self) -> u32 {
        ((u64::from(self.rate) << 16) / u64::from(self.packets_per_second)) as u32
    }
}

/// Where the audio device is attached.
#[derive(Clone, Copy)]
enum Bus {
    Xhci { controller_index: usize, controller: (u8, u8, u8), device: Device },
    Ehci { controller_index: usize, target: ehci::Target },
}

/// Control requests on either bus.
enum Host<'a> {
    Xhci(&'a mut Controller, Device),
    Ehci(&'a mut ehci::Controller, ehci::Target),
}

impl Host<'_> {
    fn control(&mut self, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8]) -> bool {
        match self {
            Host::Xhci(controller, device) => controller.control_transfer(device, request_type, request, value, index, data).is_ok(),
            Host::Ehci(controller, target) => controller.control_transfer(target, request_type, request, value, index, data).is_ok(),
        }
    }
}

struct AudioDevice {
    bus: Bus,
    description: Description,
    /// Rates the device offers (class 1.0: from the descriptor; class 2.0: the clock source's ranges).
    rates: [u32; 8],
    rate_count: usize,
    capture: Option<Stream>,
    playback: Option<Stream>,
}

/// How many USB audio devices can be kept at once ("units", numbered from 0 in the order they were attached). Each has its own
/// streams and picked modes, so one can capture while another plays.
pub const UNITS: usize = 2;

static mut DEVICES: [Option<AudioDevice>; UNITS] = [const { None }; UNITS];

fn audio(unit: usize) -> Option<&'static mut AudioDevice> {
    unsafe { (*core::ptr::addr_of_mut!(DEVICES)).get_mut(unit)?.as_mut() }
}

/// The units that hold a device.
fn units() -> impl Iterator<Item = usize> {
    (0..UNITS).filter(|&unit| audio(unit).is_some())
}

/// The unit for a device attaching on an xHCI controller: the one that holds the same port (the device was unplugged and came
/// back; its old entry is dead), else a free one.
fn xhci_unit(controller_index: usize, new: &Device) -> Option<usize> {
    let same = units().find(|&unit| match audio(unit).map(|device| device.bus) {
        Some(Bus::Xhci { controller_index: index, device, .. }) => index == controller_index && (device.port, device.route, device.tier) == (new.port, new.route, new.tier),
        _ => false,
    });
    same.or_else(|| (0..UNITS).find(|&unit| audio(unit).is_none()))
}

/// Keeps an audio device after enumeration (its slot stays open) and reads its sample rates.
pub fn attach(controller: &mut Controller, device: Device, description: Description, configuration_value: u8) -> bool {
    let Some(unit) = xhci_unit(controller.index, &device) else {
        crate::println!("USB audio: {UNITS} audio devices are already attached; this one is ignored.");
        return false;
    };
    if controller.set_configuration(&device, configuration_value).is_err() {
        return false;
    }
    let bus = Bus::Xhci { controller_index: controller.index, controller: (controller.pci.bus, controller.pci.device, controller.pci.function), device };
    install(unit, bus, Host::Xhci(controller, device), description);
    true
}

/// The same for a device behind an EHCI controller. Only high-speed devices stream (a full-speed isochronous endpoint behind a
/// hub needs split isochronous descriptors, which this driver does not build). The controller has one iTD ring per direction,
/// so a second device on the same EHCI controller is refused (put it on another controller, or on xHCI).
pub fn attach_ehci(controller: &mut ehci::Controller, controller_index: usize, target: ehci::Target, description: Description, configuration_value: u8) -> bool {
    let busy = units().any(|unit| matches!(audio(unit).map(|device| device.bus), Some(Bus::Ehci { controller_index: index, .. }) if index == controller_index));
    let Some(unit) = (0..UNITS).find(|&unit| audio(unit).is_none()).filter(|_| !busy) else {
        crate::println!("USB audio: no free slot for another device here (one per EHCI controller, {UNITS} in all); this one is ignored.");
        return false;
    };
    if target.speed != ehci::Speed::High {
        crate::println!("USB audio: a full- or low-speed device on EHCI needs split isochronous transfers, which are not supported.");
        return false;
    }
    if description.alts[..description.alt_count].iter().any(|alt| alt.extra_transactions != 0 || alt.interval == 0 || alt.interval > 4) {
        crate::println!("USB audio: this device uses high-bandwidth or slow-interval endpoints, which the EHCI driver does not stream.");
        return false;
    }
    if controller.set_configuration(&target, configuration_value).is_err() {
        return false;
    }
    install(unit, Bus::Ehci { controller_index, target }, Host::Ehci(controller, target), description);
    true
}

fn install(unit: usize, bus: Bus, mut host: Host, description: Description) {
    let mut rates = [0u32; 8];
    let mut rate_count = 0;
    let add = |rate: u32, rates: &mut [u32; 8], count: &mut usize| {
        if rate != 0 && *count < rates.len() && !rates[..*count].contains(&rate) {
            rates[*count] = rate;
            *count += 1;
        }
    };
    if description.class_version == 2 && description.clock_count > 0 {
        // GET RANGE (request 2) on the clock source's sampling-frequency control: a count, then
        // (minimum, maximum, resolution) triplets of 32-bit values.
        let mut data = [0u8; 2 + 12 * 8];
        let index = (u16::from(description.clock_sources[0]) << 8) | u16::from(description.control_interface);
        if host.control(0xa1, 0x02, 0x0100, index, &mut data) {
            let ranges = usize::from(u16::from_le_bytes([data[0], data[1]])).min(8);
            for range in 0..ranges {
                let at = 2 + 12 * range;
                let (low, high, step) = (
                    u32::from_le_bytes(data[at..at + 4].try_into().unwrap()),
                    u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap()),
                    u32::from_le_bytes(data[at + 8..at + 12].try_into().unwrap()),
                );
                if low == high {
                    add(low, &mut rates, &mut rate_count); // a single rate
                } else if step != 0 {
                    // A real range: offer the standard rates inside it that the step allows.
                    for standard in [44_100u32, 48_000, 88_200, 96_000, 176_400, 192_000] {
                        if standard >= low && standard <= high && (standard - low) % step == 0 {
                            add(standard, &mut rates, &mut rate_count);
                        }
                    }
                }
            }
        }
    } else {
        for alt in &description.alts[..description.alt_count] {
            for &rate in &alt.rates[..alt.rate_count] {
                add(rate, &mut rates, &mut rate_count);
            }
        }
    }
    unsafe {
        (*core::ptr::addr_of_mut!(DEVICES))[unit] = Some(AudioDevice { bus, description, rates, rate_count, capture: None, playback: None });
        (*core::ptr::addr_of_mut!(SELECTED))[unit] = [None, None];
    }
}

pub fn present(unit: usize) -> bool {
    audio(unit).is_some()
}

/// Where a unit is attached, for `audio` listings: "USB audio on xHCI port 3" or "USB audio on EHCI 00:1a.0".
pub fn label(unit: usize) -> impl core::fmt::Display {
    struct Label(Option<Bus>);
    impl core::fmt::Display for Label {
        fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
            match self.0 {
                Some(Bus::Xhci { device, .. }) => write!(f, "USB audio on xHCI port {}", device.path()),
                Some(Bus::Ehci { controller_index, target }) => write!(f, "USB audio on EHCI {controller_index}, address {}", target.address),
                None => write!(f, "USB audio"),
            }
        }
    }
    Label(audio(unit).map(|device| device.bus))
}

/// The mode (alternate setting, by index into the description) the user picked for capture and for playback, per unit; None
/// picks automatically. Set by `select`.
static mut SELECTED: [[Option<usize>; 2]; UNITS] = [[None, None]; UNITS];

fn direction(capture: bool) -> usize {
    usize::from(!capture)
}

/// The alternate settings that carry audio in one direction, in the device's order: these are the "modes" a user picks from.
fn modes(description: &Description, capture: bool) -> impl Iterator<Item = (usize, &StreamAlt)> {
    description.alts[..description.alt_count].iter().enumerate().filter(move |(_, alt)| (alt.endpoint & 0x80 != 0) == capture)
}

/// Whether a mode can run at `rate`. Class 1.0 lists its rates per mode (a PCM2902 records mono and stereo at several rates
/// but plays at three); class 2.0 has one clock for the whole device.
fn offers(device_rates: &[u32], class_version: u8, alt: &StreamAlt, rate: u32) -> bool {
    if class_version == 2 || (alt.rate_count == 0 && alt.continuous_range == (0, 0)) {
        return device_rates.is_empty() || device_rates.contains(&rate);
    }
    if alt.rate_count > 0 { alt.rates[..alt.rate_count].contains(&rate) } else { rate >= alt.continuous_range.0 && rate <= alt.continuous_range.1 }
}

/// The mode to start at `rate`: the one the user picked (it must offer the rate), else the best that does: most channels,
/// then most bits.
fn choose_alt(unit: usize, device: &AudioDevice, capture: bool, rate: u32) -> Result<StreamAlt, &'static str> {
    let rates = &device.rates[..device.rate_count];
    let class = device.description.class_version;
    if let Some(index) = unsafe { (*core::ptr::addr_of!(SELECTED))[unit][direction(capture)] } {
        let alt = device.description.alts[index];
        return if offers(rates, class, &alt, rate) { Ok(alt) } else { Err("the chosen mode does not offer that sample rate (`audio modes` lists what each offers)") };
    }
    if modes(&device.description, capture).next().is_none() {
        return Err(if capture { "the device has no capture stream" } else { "the device has no playback stream" });
    }
    modes(&device.description, capture)
        .filter(|(_, alt)| offers(rates, class, alt, rate))
        .max_by_key(|(_, alt)| (alt.channels, alt.bits, !alt.float))
        .map(|(_, alt)| *alt)
        .ok_or(if capture { "no capture mode of the device offers that sample rate" } else { "no playback mode of the device offers that sample rate" })
}

/// The mode a tape sees before it has started anything: the picked one, else the best at 48 kHz (or at any rate).
fn default_alt(unit: usize, device: &AudioDevice, capture: bool) -> Option<StreamAlt> {
    let rates = &device.rates[..device.rate_count];
    let class = device.description.class_version;
    if let Some(index) = unsafe { (*core::ptr::addr_of!(SELECTED))[unit][direction(capture)] } {
        return Some(device.description.alts[index]);
    }
    modes(&device.description, capture).max_by_key(|(_, alt)| (offers(rates, class, alt, 48_000), alt.channels, alt.bits, !alt.float)).map(|(_, alt)| *alt)
}

/// `mode` is the 1-based number `audio modes` shows for that direction; 0 means automatic. It applies to the next start (a
/// running stream keeps its mode).
pub fn select(unit: usize, capture: bool, mode: u32) -> Result<(), &'static str> {
    let device = audio(unit).ok_or("no USB audio device")?;
    let index = if mode == 0 {
        None
    } else {
        Some(modes(&device.description, capture).nth(mode as usize - 1).map(|(index, _)| index).ok_or("no such mode (`audio modes` lists them)")?)
    };
    unsafe { (*core::ptr::addr_of_mut!(SELECTED))[unit][direction(capture)] = index };
    Ok(())
}

/// `audio modes`: what the device offers in each direction, numbered, with the picked one marked.
pub fn print_modes(unit: usize) {
    let Some(device) = audio(unit) else { return crate::println!("No USB audio device.") };
    for (capture, name) in [(false, "Playback"), (true, "Capture")] {
        let picked = unsafe { (*core::ptr::addr_of!(SELECTED))[unit][direction(capture)] };
        crate::println!("{name} modes (`audio use {} N`, or `auto`):", if capture { "capture" } else { "playback" });
        let mut any = false;
        for (number, (index, alt)) in modes(&device.description, capture).enumerate() {
            any = true;
            crate::print(format_args!("  {}{:>2}: {} ch, {}{}-bit,", if picked == Some(index) { '*' } else { ' ' }, number + 1, alt.channels, if alt.float { "float " } else { "" }, alt.bits));
            if device.description.class_version == 2 || alt.rate_count == 0 && alt.continuous_range == (0, 0) {
                crate::print(format_args!(" the device's rates"));
            } else if alt.rate_count > 0 {
                for rate in &alt.rates[..alt.rate_count] {
                    crate::print(format_args!(" {rate}"));
                }
            } else {
                crate::print(format_args!(" {}-{}", alt.continuous_range.0, alt.continuous_range.1));
            }
            crate::println!(" Hz, {}", ["no sync", "asynchronous", "adaptive", "synchronous"][usize::from(alt.sync & 3)]);
        }
        if !any {
            crate::println!("  none");
        }
        if picked.is_none() && any {
            crate::println!("  (automatic: the mode with the most channels and bits that offers the rate asked for; integer before float)");
        }
    }
}

/// Selects the sample rate: class 1.0 sets it on the streaming endpoint (SET_CUR, 3 bytes), class 2.0
/// on the clock source (SET_CUR, 4 bytes).
fn set_rate(host: &mut Host, description: &Description, alt: &StreamAlt, rate: u32) -> bool {
    let bytes = rate.to_le_bytes();
    if description.class_version == 2 {
        let index = (u16::from(description.clock_sources[0]) << 8) | u16::from(description.control_interface);
        let mut data = bytes;
        host.control(0x21, 0x01, 0x0100, index, &mut data)
    } else {
        let mut data = [bytes[0], bytes[1], bytes[2]];
        host.control(0x22, 0x01, 0x0100, u16::from(alt.endpoint), &mut data)
    }
}

/// Selects the streaming alternate setting and the sample rate.
fn select_alt(host: &mut Host, description: &Description, alt: &StreamAlt, rate: u32) -> Result<(), &'static str> {
    if !host.control(0x01, 0x0b, u16::from(alt.alt), u16::from(alt.interface), &mut []) {
        return Err("SET_INTERFACE failed");
    }
    // A class 1.0 alternate setting that offers exactly one rate (a fixed-rate device such as a PCM2902) has nothing to
    // select, and such devices stall the request: skip it instead of warning on every start.
    let fixed_rate = description.class_version != 2 && alt.rate_count == 1 && alt.rates[0] == rate;
    if !fixed_rate && !set_rate(host, description, alt, rate) {
        crate::println!("audio: the device refused sample rate {rate} (carrying on at whatever it uses).");
    }
    Ok(())
}

/// Everything a stream needs, built with interrupts enabled and the stream not yet visible to `irq_event`.
fn build_stream(controller: &mut Controller, device: &Device, description: &Description, alt: StreamAlt, rate: u32, capture: bool) -> Result<Stream, &'static str> {
    if alt.max_packet == 0 {
        return Err("the endpoint has no packet size");
    }
    let frames = ((WINDOW * usize::from(alt.max_packet)).div_ceil(4096)) as u64;
    let buffers = memory::allocate_contiguous(frames, Some(1 << 32)).ok_or("no memory for the packet buffers")?;
    let Some(pcm) = (if capture { allocate_capture_ring() } else { PcmRing::allocate(RING_BYTES) }) else {
        memory::free(buffers, frames);
        return Err("no memory for the PCM ring");
    };
    let Some(ring) = TransferRing::allocate() else {
        memory::free(buffers, frames);
        pcm.free();
        return Err("no memory for the transfer ring");
    };
    let cleanup = |ring: &TransferRing, pcm: &PcmRing| {
        memory::free(buffers, frames);
        memory::free(ring.base(), 1);
        pcm.free();
    };
    if let Err(reason) = select_alt(&mut Host::Xhci(&mut *controller, *device), description, &alt, rate) {
        cleanup(&ring, &pcm);
        return Err(reason);
    }
    let dci = match controller.configure_isoc_endpoint(device, alt.endpoint, alt.max_packet, alt.interval, &ring) {
        Ok(dci) => dci,
        Err(_) => {
            cleanup(&ring, &pcm);
            return Err("Configure Endpoint failed");
        }
    };
    let interval_shift = u32::from(alt.interval.clamp(1, 16)) - 1;
    let packets_per_second = if device.speed >= 3 { 8000 >> interval_shift } else { 1000 >> interval_shift };
    let mut stream = Stream {
        alt,
        dci,
        capture,
        ring: Some(ring),
        doorbell: controller.doorbell_address(device.slot),
        buffers,
        frames,
        ehci: None,
        next: 0,
        trbs: [0; WINDOW],
        window: WINDOW,
        pcm,
        step_q16: 0,
        accumulator_q16: 0,
        feedback: None,
        packets_per_second: packets_per_second.max(1),
        rate,
        active: AtomicBool::new(false),
        packets: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
        monitor: false,
        mon_frames: 0,
        mon_packets: 0,
    };
    stream.step_q16 = stream.nominal_step();
    if !capture {
        stream.window = ((stream.packets_per_second * PLAYBACK_WINDOW_MS / 1000) as usize).clamp(8, WINDOW);
    }
    if !capture && alt.feedback != 0 && alt.feedback_packet >= 3 {
        stream.feedback = build_feedback(controller, device, &alt);
    }
    Ok(stream)
}

/// The feedback endpoint (an isochronous IN endpoint). Optional: if anything fails the stream simply
/// runs at the nominal rate.
fn build_feedback(controller: &mut Controller, device: &Device, alt: &StreamAlt) -> Option<Feedback> {
    let buffers = memory::allocate_contiguous(1, Some(1 << 32))?;
    let Some(ring) = TransferRing::allocate() else {
        memory::free(buffers, 1);
        return None;
    };
    let packet = alt.feedback_packet.min(4);
    match controller.configure_isoc_endpoint(device, alt.feedback, packet, alt.feedback_interval.max(1), &ring) {
        Ok(dci) => Some(Feedback {
            dci,
            ring,
            doorbell: controller.doorbell_address(device.slot),
            buffers,
            trbs: [0; FEEDBACK_WINDOW],
            next: 0,
            packet,
            last_q16: 0,
            min_q16: u32::MAX,
            max_q16: 0,
            received: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
        }),
        Err(_) => {
            memory::free(buffers, 1);
            memory::free(ring.base(), 1);
            None
        }
    }
}

/// Fills (playback) or just aims (capture) packet slot `slot` and queues its transfer. Interrupt context.
fn queue_slot(stream: &mut Stream, slot: usize) {
    let address = stream.buffers + (slot * usize::from(stream.alt.max_packet)) as u64;
    let length = if stream.capture { u32::from(stream.alt.max_packet) } else { fill_playback_packet(stream, address) as u32 };
    if let Some(ring) = stream.ring.as_mut() {
        stream.trbs[slot] = xhci::push_isoc(ring, address, length);
    }
}

/// Fills a playback packet at `address` from the PCM ring (silence when it is empty, counted as an underrun) and returns
/// its length in bytes. Interrupt context.
/// Diagnostics: the packet counts at the first and the latest underrun of the last playback stream.
/// Diagnostics: the frames-per-packet step (Q16) measured from the device's capture data, 0 if none.
static MEASURED_STEP: AtomicU32 = AtomicU32::new(0);
static UNDERRUN_FIRST: AtomicU64 = AtomicU64::new(0);
static UNDERRUN_LAST: AtomicU64 = AtomicU64::new(0);

fn fill_playback_packet(stream: &mut Stream, address: u64) -> usize {
    // This packet's frame count: the Q16 frames-per-packet step (nominal rate / packets per second, or
    // the device's own figure from its feedback endpoint), with the fraction carried over so 44.1 kHz
    // comes out as 44 frames per packet with a 45 once in ten.
    let total = u64::from(stream.accumulator_q16) + u64::from(stream.step_q16);
    let frames = (total >> 16) as u32;
    stream.accumulator_q16 = (total & 0xffff) as u32;
    let frame_bytes = usize::from(stream.alt.channels) * usize::from(stream.alt.subslot_bytes);
    let wanted = (frames as usize * frame_bytes).min(usize::from(stream.alt.max_packet));
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, wanted) };
    let got = stream.pcm.pop(buffer);
    if got < wanted {
        buffer[got..].fill(0);
        if stream.underruns.fetch_add(1, Ordering::Relaxed) == 0 {
            UNDERRUN_FIRST.store(stream.packets.load(Ordering::Relaxed), Ordering::Relaxed);
        }
        UNDERRUN_LAST.store(stream.packets.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    wanted
}

/// Handles one feedback transfer event of `stream`. Interrupt context.
fn feedback_event(stream: &mut Stream, event: &xhci::Trb, high_speed: bool) -> bool {
    let nominal = stream.nominal_step();
    let packets_per_second = stream.packets_per_second.max(1);
    let Some(feedback) = stream.feedback.as_mut() else { return false };
    if event.endpoint_index() != feedback.dci {
        return false;
    }
    let slot = if feedback.trbs[feedback.next] == event.trb_address() { feedback.next } else { feedback.trbs.iter().position(|&a| a == event.trb_address()).unwrap_or(feedback.next) };
    feedback.next = (slot + 1) % FEEDBACK_WINDOW;
    let length = usize::from(feedback.packet).saturating_sub(event.residual() as usize);
    let mut new_step = None;
    if matches!(event.completion(), xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT) && (length == 3 || length == 4) {
        let bytes = unsafe { core::slice::from_raw_parts((feedback.buffers + (slot * 16) as u64) as *const u8, length) };
        let mut value = 0u64;
        for (index, &byte) in bytes.iter().enumerate() {
            value |= u64::from(byte) << (8 * index);
        }
        // 3 bytes: 10.14 (samples per 1 ms frame); 4 bytes: 16.16 (samples per microframe). Convert to
        // Q16 samples per *packet*, a packet being 2^(bInterval-1) such frames.
        let per_frame_q16 = if length == 3 { value << 2 } else { value };
        let frames_per_packet = u64::from(if high_speed { 8000 } else { 1000 } / packets_per_second);
        let step = per_frame_q16 * frames_per_packet;
        // Believe only values within 10% of the nominal rate: a garbage report must not wreck the stream.
        if step >= u64::from(nominal) * 9 / 10 && step <= u64::from(nominal) * 11 / 10 {
            feedback.last_q16 = step as u32;
            feedback.min_q16 = feedback.min_q16.min(step as u32);
            feedback.max_q16 = feedback.max_q16.max(step as u32);
            feedback.received.fetch_add(1, Ordering::Relaxed);
            new_step = Some(step as u32);
        } else {
            feedback.rejected.fetch_add(1, Ordering::Relaxed);
        }
    }
    feedback.trbs[slot] = xhci::push_isoc(&mut feedback.ring, feedback.buffers + (slot * 16) as u64, u32::from(feedback.packet));
    xhci::ring_doorbell_at(feedback.doorbell, feedback.dci);
    if let Some(step) = new_step {
        stream.step_q16 = step;
    }
    true
}

/// Called by the xHCI event drain (interrupt context, or the foreground with interrupts off) for every
/// event; returns true if it belonged to an audio stream and was handled. Must not allocate, print or wait.
pub fn irq_event(index: usize, event: &xhci::Trb) -> bool {
    event.is_transfer() && (0..UNITS).any(|unit| unit_irq_event(unit, index, event))
}

fn unit_irq_event(unit: usize, index: usize, event: &xhci::Trb) -> bool {
    let Some(device) = audio(unit) else { return false };
    let Bus::Xhci { controller_index, device: xhci_device, .. } = device.bus else { return false };
    if controller_index != index || event.slot_number() != xhci_device.slot {
        return false;
    }
    let high_speed = xhci_device.speed >= 3;
    for stream in [device.capture.as_mut(), device.playback.as_mut()].into_iter().flatten() {
        let ours = event.endpoint_index() == stream.dci || stream.feedback.as_ref().is_some_and(|feedback| feedback.dci == event.endpoint_index());
        if !ours {
            continue;
        }
        if !stream.active.load(Ordering::Acquire) {
            return true; // a stream being set up or torn down: its events are of no interest
        }
        if !stream.capture && feedback_event(stream, event, high_speed) {
            return true;
        }
        // Completions arrive in queue order; if one was missed, resynchronise on the TRB address.
        let slot = if stream.trbs[stream.next] == event.trb_address() { stream.next } else { stream.trbs.iter().position(|&a| a == event.trb_address()).unwrap_or(stream.next) };
        stream.next = (slot + 1) % stream.window;
        stream.packets.fetch_add(1, Ordering::Relaxed);
        let ok = matches!(event.completion(), xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT);
        if !ok {
            stream.errors.fetch_add(1, Ordering::Relaxed);
        }
        if stream.capture && ok {
            let address = stream.buffers + (slot * usize::from(stream.alt.max_packet)) as u64;
            let length = usize::from(stream.alt.max_packet).saturating_sub(event.residual() as usize);
            let data = unsafe { core::slice::from_raw_parts(address as *const u8, length) };
            stream.bytes.fetch_add(length as u64, Ordering::Relaxed);
            stream.pcm.push_whole(data);
        }
        queue_slot(stream, slot);
        xhci::ring_doorbell_at(stream.doorbell, stream.dci);
        return true;
    }
    false
}

/// Publishes a built stream and queues its first window, all with interrupts off, so `irq_event` sees
/// it complete or not at all, and cannot push to its ring while the first window is being queued.
fn publish(slot: &mut Option<Stream>, stream: Stream) {
    let was = crate::cpu::interrupts_save_and_disable();
    *slot = Some(stream);
    let stream = slot.as_mut().unwrap();
    for position in 0..stream.window {
        queue_slot(stream, position);
    }
    // The first window goes out before the caller can have written anything: silence by construction, not an underrun.
    stream.underruns.store(0, Ordering::Relaxed);
    if let Some(feedback) = stream.feedback.as_mut() {
        for position in 0..FEEDBACK_WINDOW {
            feedback.trbs[position] = xhci::push_isoc(&mut feedback.ring, feedback.buffers + (position * 16) as u64, u32::from(feedback.packet));
        }
        xhci::ring_doorbell_at(feedback.doorbell, feedback.dci);
    }
    stream.active.store(true, Ordering::Release);
    xhci::ring_doorbell_at(stream.doorbell, stream.dci);
    crate::cpu::interrupts_restore(was);
}

/// Starts capture and/or playback at `rate`. Returns a reason on failure.
pub fn start(unit: usize, rate: u32, want_capture: bool, want_playback: bool) -> Result<(), &'static str> {
    let Some(device) = audio(unit) else { return Err("no USB audio device") };
    if device.capture.is_some() || device.playback.is_some() {
        return Err("already running");
    }
    if device.rate_count > 0 && !device.rates[..device.rate_count].contains(&rate) {
        return Err("the device does not offer that sample rate");
    }
    let bus = device.bus;
    let description = Description { ..clone_description(&device.description) };
    for capture in [true, false] {
        let monitor = capture && !want_capture && want_playback && matches!(bus, Bus::Ehci { .. });
        if capture && !want_capture && !monitor || !capture && !want_playback {
            continue;
        }
        let alt = match choose_alt(unit, device, capture, rate) {
            Ok(alt) => alt,
            Err(_) if monitor => continue,
            Err(reason) => return Err(reason),
        };
        let result = match bus {
            Bus::Xhci { device: xhci_device, .. } => {
                let controller = controller(unit).ok_or("no USB audio device")?;
                build_stream(controller, &xhci_device, &description, alt, rate, capture).map(|stream| publish(if capture { &mut device.capture } else { &mut device.playback }, stream))
            }
            Bus::Ehci { controller_index, target } => {
                let controller = ehci::controller(controller_index).ok_or("no USB audio device")?;
                build_ehci_stream(controller, controller_index, target, &description, alt, rate, capture, monitor)
                    .map(|stream| publish_ehci(controller, if capture { &mut device.capture } else { &mut device.playback }, stream))
            }
        };
        match result {
            Err(_) if monitor => {}
            other => other?,
        }
    }
    Ok(())
}

/// Everything an EHCI stream needs: the packet buffers (one page-aligned region per iTD), the iTD ring linked into the
/// controller's periodic list, the PCM ring. Built with interrupts enabled and the stream not yet visible to
/// `ehci_event`. Memory stays below 256 MiB, where the first real EHCI is known to work (see `ehci.rs`).
fn build_ehci_stream(controller: &mut ehci::Controller, controller_index: usize, target: ehci::Target, description: &Description, alt: StreamAlt, rate: u32, capture: bool, monitor: bool) -> Result<Stream, &'static str> {
    if alt.max_packet == 0 || alt.max_packet > 1024 {
        return Err("the endpoint has no usable packet size");
    }
    let step = 1usize << (alt.interval.clamp(1, 4) - 1);
    let per_itd = 8 / step;
    let region = (per_itd * usize::from(alt.max_packet)).next_multiple_of(4096);
    let frames = (ehci::ISO_RING * region / 4096) as u64;
    let low = Some(256u64 << 20);
    let buffers = memory::allocate_contiguous(frames, low).ok_or("no memory for the packet buffers")?;
    let Some(itds) = memory::allocate_contiguous(1, low) else {
        memory::free(buffers, frames);
        return Err("no memory for the descriptors");
    };
    let Some(pcm) = (if capture && !monitor { allocate_capture_ring() } else { PcmRing::allocate(RING_BYTES) }) else {
        memory::free(buffers, frames);
        memory::free(itds, 1);
        return Err("no memory for the PCM ring");
    };
    let cleanup = |pcm: &PcmRing| {
        memory::free(buffers, frames);
        memory::free(itds, 1);
        pcm.free();
    };
    if let Err(reason) = select_alt(&mut Host::Ehci(&mut *controller, target), description, &alt, rate) {
        cleanup(&pcm);
        return Err(reason);
    }
    for k in 0..ehci::ISO_RING {
        ehci::itd_init(itds as usize + k * ehci::ITD_BYTES, &target, alt.endpoint, alt.max_packet, capture, buffers + (k * region) as u64, region / 4096);
    }
    let which = usize::from(capture);
    let mut stream = Stream {
        alt,
        dci: 0,
        capture,
        ring: None,
        doorbell: 0,
        buffers,
        frames,
        ehci: Some(EhciPipe { controller_index, which, itds, next: 0, step, per_itd, region }),
        next: 0,
        trbs: [0; WINDOW],
        window: ehci::ISO_RING,
        pcm,
        step_q16: 0,
        accumulator_q16: 0,
        feedback: None,
        packets_per_second: (8000 / step as u32).max(1),
        rate,
        active: AtomicBool::new(false),
        packets: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        errors: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
        monitor: false,
        mon_frames: 0,
        mon_packets: 0,
    };
    stream.step_q16 = stream.nominal_step();
    stream.monitor = monitor;
    Ok(stream)
}

/// Fills (playback) or aims (capture) the iTD at ring position `k` and arms it. Interrupt context.
fn ehci_fill(stream: &mut Stream, k: usize) {
    let Some(pipe) = stream.ehci else { return };
    let region = stream.buffers + (k * pipe.region) as u64;
    let max_packet = usize::from(stream.alt.max_packet);
    let mut lengths = [0u16; 8];
    for (j, length) in lengths[..pipe.per_itd].iter_mut().enumerate() {
        *length = if stream.capture { max_packet as u16 } else { fill_playback_packet(stream, region + (j * max_packet) as u64) as u16 };
    }
    ehci::itd_arm(pipe.itds as usize + k * ehci::ITD_BYTES, pipe.step, &lengths[..pipe.per_itd], stream.alt.max_packet);
}

/// Links the iTD ring into the periodic list, then (interrupts off, so `ehci_event` sees the stream complete or not at all)
/// arms every iTD in the order the controller will visit them, starting two frames from now.
fn publish_ehci(controller: &mut ehci::Controller, slot: &mut Option<Stream>, stream: Stream) {
    let Some(pipe) = stream.ehci else { return };
    if !controller.enable_interrupts(pipe.controller_index) {
        crate::println!("audio: no interrupt path for EHCI; the descriptors are serviced from the foreground (`audio::poll`).");
    }
    controller.iso_install(pipe.which, pipe.itds as usize);
    let was = crate::cpu::interrupts_save_and_disable();
    *slot = Some(stream);
    let stream = slot.as_mut().unwrap();
    let first = (controller.frame_now() + 2) % ehci::ISO_RING;
    if let Some(pipe) = stream.ehci.as_mut() {
        pipe.next = first;
    }
    for offset in 0..ehci::ISO_RING {
        ehci_fill(stream, (first + offset) % ehci::ISO_RING);
    }
    stream.active.store(true, Ordering::Release);
    crate::cpu::interrupts_restore(was);
}

/// Completed iTDs of one stream: collect what a capture iTD received, refill and re-arm it, move on. An iTD is visited once
/// every `ISO_RING` frames; one that is still active although its frame has passed was missed (the schedule stopped, or
/// this ran too late): it is counted and re-armed anyway. Interrupt context.
/// Diagnostics of the EHCI capture path, per iTD ring position: bytes the controller reported, and how many of them were non-zero.
static ISO_LEN: [AtomicU64; ehci::ISO_RING] = [const { AtomicU64::new(0) }; ehci::ISO_RING];
static ISO_NONZERO: [AtomicU64; ehci::ISO_RING] = [const { AtomicU64::new(0) }; ehci::ISO_RING];

fn ehci_service(stream: &mut Stream, frame: usize) {
    for _ in 0..ehci::ISO_RING {
        let Some(pipe) = stream.ehci else { return };
        let itd = pipe.itds as usize + pipe.next * ehci::ITD_BYTES;
        let since = (frame + ehci::ISO_RING - pipe.next) % ehci::ISO_RING;
        if ehci::itd_active(itd, pipe.step, pipe.per_itd) {
            if since == 0 || since > ehci::ISO_RING / 2 {
                return; // running now, or not due yet
            }
            stream.errors.fetch_add(1, Ordering::Relaxed);
        } else if stream.capture {
            let region = stream.buffers + (pipe.next * pipe.region) as u64;
            for j in 0..pipe.per_itd {
                let (length, errors) = ehci::itd_result(itd, pipe.step, j);
                if errors != 0 {
                    stream.errors.fetch_add(1, Ordering::Relaxed);
                } else if length > 0 && stream.monitor {
                    stream.mon_frames += (length / (usize::from(stream.alt.channels) * usize::from(stream.alt.subslot_bytes)).max(1)) as u64;
                } else if length > 0 {
                    let data = unsafe { core::slice::from_raw_parts((region + (j * usize::from(stream.alt.max_packet)) as u64) as *const u8, length) };
                    stream.bytes.fetch_add(length as u64, Ordering::Relaxed);
                    ISO_LEN[pipe.next].fetch_add(length as u64, Ordering::Relaxed);
                    ISO_NONZERO[pipe.next].fetch_add(data.iter().filter(|&&b| b != 0).count() as u64, Ordering::Relaxed);
                    stream.pcm.push_whole(data);
                }
            }
        } else {
            for j in 0..pipe.per_itd {
                if ehci::itd_result(itd, pipe.step, j).1 != 0 {
                    stream.errors.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        stream.packets.fetch_add(pipe.per_itd as u64, Ordering::Relaxed);
        stream.mon_packets += pipe.per_itd as u64;
        ehci_fill(stream, pipe.next);
        if let Some(pipe) = stream.ehci.as_mut() {
            pipe.next = (pipe.next + 1) % ehci::ISO_RING;
        }
    }
}

/// The EHCI completion interrupt (or `ehci_poll`): services the running streams. Must not allocate, print or wait.
pub fn ehci_event(index: usize, frame: usize) {
    for unit in 0..UNITS {
        unit_ehci_event(unit, index, frame);
    }
}

fn unit_ehci_event(unit: usize, index: usize, frame: usize) {
    let Some(device) = audio(unit) else { return };
    let Bus::Ehci { controller_index, .. } = device.bus else { return };
    if controller_index != index {
        return;
    }
    for stream in [device.capture.as_mut(), device.playback.as_mut()].into_iter().flatten() {
        if stream.active.load(Ordering::Acquire) {
            ehci_service(stream, frame);
        }
    }
    // Implicit feedback: an asynchronous device without a feedback endpoint consumes at its own clock, which the
    // frames it delivers per packet reveal (it sends one sample per tick of that clock). The running average over
    // the whole stream becomes the playback stream's frames-per-packet step (Q16), within 1% of nominal.
    if let (Some(monitor), Some(playback)) = (device.capture.as_ref(), device.playback.as_mut()) {
        if monitor.monitor && monitor.mon_packets >= 4000 {
            let measured = ((monitor.mon_frames << 16) / monitor.mon_packets) as u32;
            let nominal = playback.nominal_step();
            if measured > nominal - nominal / 100 && measured < nominal + nominal / 100 {
                playback.step_q16 = measured;
                MEASURED_STEP.store(measured, Ordering::Relaxed);
            }
        }
    }
}

/// Foreground servicing for an EHCI audio device whose interrupt could not be routed.
pub fn ehci_poll() {
    for unit in 0..UNITS {
        poll_unit(unit);
    }
}

fn poll_unit(unit: usize) {
    let Some(device) = audio(unit) else { return };
    let Bus::Ehci { controller_index, .. } = device.bus else { return };
    if device.capture.is_none() && device.playback.is_none() {
        return;
    }
    let Some(controller) = ehci::controller(controller_index) else { return };
    if controller.interrupts_enabled() {
        return;
    }
    let was = crate::cpu::interrupts_save_and_disable();
    unit_ehci_event(unit, controller_index, controller.frame_now());
    crate::cpu::interrupts_restore(was);
}

/// The device at hub port `hub_port` of hub `hub_address` (or on root port: hub_address 0) of an EHCI controller went away: forget
/// it if it was the audio device.
pub fn ehci_departed(controller_index: usize, hub_address: u8, hub_port: u8) {
    for unit in 0..UNITS {
        let Some(device) = audio(unit) else { continue };
        let Bus::Ehci { controller_index: index, target } = device.bus else { continue };
        if index != controller_index || target.hub_address != hub_address || (hub_port != 0 && target.hub_port != hub_port) {
            continue;
        }
        stop(unit);
        unsafe { (*core::ptr::addr_of_mut!(DEVICES))[unit] = None };
    }
}

fn clone_description(description: &Description) -> Description {
    Description {
        class_version: description.class_version,
        control_interface: description.control_interface,
        clock_sources: description.clock_sources,
        clock_count: description.clock_count,
        alts: description.alts,
        alt_count: description.alt_count,
    }
}

fn stop_stream(host: &mut Host, mut stream: Stream) {
    if !stream.capture {
        // What the stream did, for diagnosing underruns after a run (frames per packet, Q16 shown as milli-frames).
        let milli = |value: u32| (u64::from(value) * 1000) >> 16;
        crate::print(format_args!(
            "usb audio playback: {} packets, {} underruns, nominal {} mf/packet",
            stream.packets.load(Ordering::Relaxed),
            stream.underruns.load(Ordering::Relaxed),
            milli(stream.nominal_step())
        ));
        let measured = MEASURED_STEP.swap(0, Ordering::Relaxed);
        if measured != 0 {
            crate::print(format_args!(" (measured {} mf/packet = {} ppm vs nominal)", milli(measured), (i64::from(measured) - i64::from(stream.nominal_step())) * 1_000_000 / i64::from(stream.nominal_step())));
        }
        if stream.underruns.load(Ordering::Relaxed) > 0 {
            crate::print(format_args!(" (first at packet {}, last at {})", UNDERRUN_FIRST.load(Ordering::Relaxed), UNDERRUN_LAST.load(Ordering::Relaxed)));
        }
        match stream.feedback.as_ref() {
            Some(feedback) if feedback.received.load(Ordering::Relaxed) > 0 => crate::println!(
                "; feedback {} reports ({} rejected), {} to {} mf/packet.",
                feedback.received.load(Ordering::Relaxed),
                feedback.rejected.load(Ordering::Relaxed),
                milli(feedback.min_q16),
                milli(feedback.max_q16)
            ),
            Some(_) => crate::println!("; feedback endpoint gave no usable report."),
            None => crate::println!("; no feedback endpoint (nominal rate used)."),
        }
    }
    match host {
        Host::Xhci(controller, device) => {
            if let Some(feedback) = stream.feedback.take() {
                controller.stop_endpoint(device.slot, feedback.dci);
                controller.drop_endpoint(device, feedback.dci);
                memory::free(feedback.buffers, 1);
                memory::free(feedback.ring.base(), 1);
            }
            controller.stop_endpoint(device.slot, stream.dci);
            controller.drop_endpoint(device, stream.dci);
            if let Some(ring) = stream.ring.as_ref() {
                memory::free(ring.base(), 1);
            }
        }
        Host::Ehci(controller, _) => {
            if let Some(pipe) = stream.ehci {
                crate::println!(
                    "ehci iso: {} packets, {} bytes, {} errors, next {}, frame {}, itd0 {:08x} {:08x} sts {:08x}",
                    stream.packets.load(Ordering::Relaxed),
                    stream.bytes.load(Ordering::Relaxed),
                    stream.errors.load(Ordering::Relaxed),
                    pipe.next,
                    controller.frame_now(),
                    unsafe { core::ptr::read_volatile(pipe.itds as *const u32) },
                    unsafe { core::ptr::read_volatile((pipe.itds + 4) as *const u32) },
                    controller.status_word()
                );
                if stream.capture {
                    for k in [0usize, 1, 14, 15] {
                        let itd = pipe.itds + (k * ehci::ITD_BYTES) as u64;
                        let word = |n: usize| unsafe { core::ptr::read_volatile((itd as usize + 4 * n) as *const u32) };
                        let buffer = stream.buffers + (k * pipe.region) as u64;
                        let head = unsafe { core::ptr::read_volatile(buffer as *const u64) };
                        crate::println!("  itd{k} @{itd:#x}: w9 {:08x} w10 {:08x} w11 {:08x} w12 {:08x}, buffer {buffer:#x} starts {head:016x}", word(9), word(10), word(11), word(12));
                    }
                    for k in 0..ehci::ISO_RING {
                        crate::println!("  iTD {k}: {} bytes, {} non-zero", ISO_LEN[k].swap(0, Ordering::Relaxed), ISO_NONZERO[k].swap(0, Ordering::Relaxed));
                    }
                }
                controller.iso_remove(pipe.which); // the schedule restarts without the ring: the controller is done with the iTDs
                memory::free(pipe.itds, 1);
            }
        }
    }
    let _ = host.control(0x01, 0x0b, 0, u16::from(stream.alt.interface), &mut []); // back to alternate setting 0
    memory::free(stream.buffers, stream.frames);
    stream.pcm.free();
}

/// Stops the streams of every unit.
pub fn stop_all() {
    for unit in 0..UNITS {
        stop(unit);
    }
}

pub fn stop(unit: usize) {
    let Some(device) = audio(unit) else { return };
    // 1. Tell `irq_event` and `ehci_event` to stop touching the streams (their events are swallowed from now on).
    let was = crate::cpu::interrupts_save_and_disable();
    for stream in [device.capture.as_ref(), device.playback.as_ref()].into_iter().flatten() {
        stream.active.store(false, Ordering::Release);
    }
    crate::cpu::interrupts_restore(was);
    // 2. Take them out, stop their endpoints (commands: interrupts must be on), release the memory.
    let was = crate::cpu::interrupts_save_and_disable();
    let capture = device.capture.take();
    let playback = device.playback.take();
    crate::cpu::interrupts_restore(was);
    match device.bus {
        Bus::Xhci { device: xhci_device, .. } => {
            let Some(controller) = controller(unit) else { return };
            for stream in [capture, playback].into_iter().flatten() {
                stop_stream(&mut Host::Xhci(&mut *controller, xhci_device), stream);
            }
        }
        Bus::Ehci { controller_index, target } => {
            let Some(controller) = ehci::controller(controller_index) else { return };
            for stream in [capture, playback].into_iter().flatten() {
                stop_stream(&mut Host::Ehci(&mut *controller, target), stream);
            }
            controller.disable_interrupts();
        }
    }
}

/// The xHCI controller the audio device is on (None for a device on EHCI).
pub fn controller(unit: usize) -> Option<&'static mut Controller> {
    let Bus::Xhci { controller: identity, .. } = audio(unit)?.bus else { return None };
    (0..xhci::count()).find_map(|index| xhci::controller(index).filter(|c| (c.pci.bus, c.pci.device, c.pci.function) == identity))
}

/// Copies captured bytes (interleaved frames) out of the PCM ring.
pub fn read(unit: usize, out: &mut [u8]) -> usize {
    audio(unit).and_then(|device| device.capture.as_ref()).map_or(0, |stream| stream.pcm.pop(out))
}

/// Queues playback bytes (interleaved frames); returns how many fitted.
pub fn write(unit: usize, data: &[u8]) -> usize {
    audio(unit).and_then(|device| device.playback.as_ref()).map_or(0, |stream| stream.pcm.push(data))
}

/// Bytes of room in the playback PCM ring.
pub fn playback_space(unit: usize) -> usize {
    audio(unit).and_then(|device| device.playback.as_ref()).map_or(0, |stream| stream.pcm.space())
}

/// Channel layout of the capture stream: (channels, bytes per sample slot).
pub fn capture_format(unit: usize) -> Option<(u8, u8)> {
    audio(unit).and_then(|device| device.capture.as_ref()).map(|stream| (stream.alt.channels, stream.alt.subslot_bytes))
}

/// Channel layout of the playback stream: (channels, bytes per sample slot).
pub fn playback_format(unit: usize) -> Option<(u8, u8)> {
    audio(unit).and_then(|device| device.playback.as_ref()).map(|stream| (stream.alt.channels, stream.alt.subslot_bytes))
}

/// Whether the capture / playback stream carries IEEE float samples.
pub fn capture_float(unit: usize) -> bool {
    audio(unit).and_then(|device| device.capture.as_ref()).is_some_and(|stream| stream.alt.float)
}

pub fn playback_float(unit: usize) -> bool {
    audio(unit).and_then(|device| device.playback.as_ref()).is_some_and(|stream| stream.alt.float)
}

/// `audio` with no arguments: what the device offers and how a running stream is doing.
pub fn status(unit: usize) {
    let Some(device) = audio(unit) else { return crate::println!("No USB audio device.") };
    let class = if device.description.class_version == 2 { "2.0" } else { "1.0" };
    crate::print(format_args!("USB audio (class {class}) sample rates:"));
    for rate in &device.rates[..device.rate_count] {
        crate::print(format_args!(" {rate}"));
    }
    crate::println!();
    for (name, stream) in [("capture", device.capture.as_ref()), ("playback", device.playback.as_ref())] {
        match stream {
            Some(stream) => crate::println!(
                "  {name}: {} Hz, {} ch {}-bit, {} packets, {} bytes, {} errors, {} underruns, ring {} bytes ({} overruns)",
                stream.rate,
                stream.alt.channels,
                stream.alt.bits,
                stream.packets.load(Ordering::Relaxed),
                stream.bytes.load(Ordering::Relaxed),
                stream.errors.load(Ordering::Relaxed),
                stream.underruns.load(Ordering::Relaxed),
                stream.pcm.available(),
                stream.pcm.overruns.load(Ordering::Relaxed)
            ),
            None => crate::println!("  {name}: stopped"),
        }
        if let Some(stream) = stream {
            if let Some(feedback) = stream.feedback.as_ref() {
                let q = |value: u32| (value >> 16, ((value & 0xffff) * 1000) >> 16);
                let (device_whole, device_fraction) = q(feedback.last_q16);
                let (nominal_whole, nominal_fraction) = q(stream.nominal_step());
                crate::println!(
                    "    feedback: {} reports, {} rejected; device clock {device_whole}.{device_fraction:03} vs nominal {nominal_whole}.{nominal_fraction:03} frames/packet",
                    feedback.received.load(Ordering::Relaxed),
                    feedback.rejected.load(Ordering::Relaxed)
                );
            }
        }
    }
}

/// Fills the tape ABI's `AudioInfo`; false if there is no audio device.
pub fn fill_info(unit: usize, info: &mut fosfor_abi::tape::AudioInfo) -> bool {
    let Some(device) = audio(unit) else { return false };
    let mut out = fosfor_abi::tape::AudioInfo::default();
    out.class_version = u32::from(device.description.class_version.max(1));
    // A running stream's mode, else the picked one or the best at 48 kHz.
    let playback_alt = device.playback.as_ref().map(|stream| stream.alt).or_else(|| default_alt(unit, device, false));
    let capture_alt = device.capture.as_ref().map(|stream| stream.alt).or_else(|| default_alt(unit, device, true));
    if let Some(alt) = playback_alt {
        out.playback_channels = u32::from(alt.channels);
        out.playback_slot_bytes = u32::from(alt.subslot_bytes);
        out.playback_bits = u32::from(alt.bits);
    }
    if let Some(alt) = capture_alt {
        out.capture_channels = u32::from(alt.channels);
        out.capture_slot_bytes = u32::from(alt.subslot_bytes);
        out.capture_bits = u32::from(alt.bits);
    }
    out.rate_count = device.rate_count as u32;
    out.rates[..device.rate_count].copy_from_slice(&device.rates[..device.rate_count]);
    if let Some(stream) = &device.capture {
        out.capture_rate = stream.rate;
        out.capture_available = stream.pcm.available() as u32;
    }
    if let Some(stream) = &device.playback {
        out.playback_rate = stream.rate;
        out.playback_space = stream.pcm.space() as u32;
    }
    *info = out;
    true
}

pub fn fill_stats(unit: usize, stats: &mut fosfor_abi::tape::AudioStats) {
    let mut out = fosfor_abi::tape::AudioStats::default();
    if let Some(device) = audio(unit) {
        if let Some(stream) = &device.capture {
            out.capture_packets = stream.packets.load(Ordering::Relaxed);
            out.capture_bytes = stream.bytes.load(Ordering::Relaxed);
            out.capture_errors = stream.errors.load(Ordering::Relaxed);
            out.capture_overruns = stream.pcm.overruns.load(Ordering::Relaxed);
        }
        if let Some(stream) = &device.playback {
            out.playback_packets = stream.packets.load(Ordering::Relaxed);
            out.playback_errors = stream.errors.load(Ordering::Relaxed);
            out.playback_underruns = stream.underruns.load(Ordering::Relaxed);
        }
    }
    *stats = out;
}

/// The capture ring's size and backlog, for `audio_buffer`.
pub fn buffer_info(unit: usize) -> (u64, u64) {
    audio(unit).and_then(|device| device.capture.as_ref()).map_or((0, 0), |stream| (stream.pcm.size as u64, stream.pcm.available() as u64))
}
