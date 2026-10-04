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
    pub endpoint: u8,
    /// Bits 3:2 of the endpoint's attributes: 0 none, 1 asynchronous, 2 adaptive, 3 synchronous.
    pub sync: u8,
    pub max_packet: u16,
    pub interval: u8,
    /// The explicit feedback endpoint of an asynchronous OUT stream, if any.
    pub feedback: u8,
    /// Class 1.0 lists its discrete sample rates in the format descriptor (0 rates = continuous range).
    pub rates: [u32; 6],
    pub rate_count: usize,
    pub continuous_range: (u32, u32),
}

pub const MAX_ALTS: usize = 16;

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
                (2, 1) if uac2_stream && length >= 11 => current.channels = d[10], // AS_GENERAL (class 2.0)
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
                    } else if current.endpoint == 0 {
                        current.endpoint = d[2];
                        current.sync = (attributes >> 2) & 3;
                        current.max_packet = u16::from_le_bytes([d[4], d[5]]) & 0x7ff;
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
            "  interface {} alt {}: {} {} ch, {}-bit in {}-byte slots, endpoint {:#04x} isochronous ({}), max packet {}, interval {}",
            alt.interface,
            alt.alt,
            if alt.endpoint & 0x80 != 0 { "capture" } else { "playback" },
            alt.channels,
            alt.bits,
            alt.subslot_bytes,
            alt.endpoint,
            sync_name(alt.sync),
            alt.max_packet,
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
// A started stream keeps a *window* of isochronous transfers queued on its endpoint, one packet
// each (a packet is what the endpoint carries per service interval: 1 ms at full speed, 125 us
// at high speed). The controller completes them in order, one event each, and the foreground
// (any call that drains the xHCI event ring: `usb_hid::poll` from the tape loops) handles the
// event: capture appends the received bytes to a PCM ring, playback fills the packet from a PCM
// ring (silence when it is empty, counted as an underrun) and both put the packet's slot back at
// the end of the window. Nothing here sleeps or blocks; a window of ~25 ms (high speed) / 190 ms
// (full speed) absorbs the gaps between polls.

use crate::xhci::{self, Controller, Device};
use crate::memory;

/// Transfers kept queued per stream.
const WINDOW: usize = 192;
/// PCM ring size in bytes (about a second of 6-channel 24-bit audio at 48 kHz fits).
const RING_BYTES: usize = 1 << 20;

pub struct Ring {
    base: u64,
    head: usize, // next byte to write
    tail: usize, // next byte to read
    pub overruns: u64,
}

impl Ring {
    const fn empty() -> Ring {
        Ring { base: 0, head: 0, tail: 0, overruns: 0 }
    }
    fn allocate() -> Option<Ring> {
        Some(Ring { base: memory::allocate_contiguous((RING_BYTES / 4096) as u64, None)?, head: 0, tail: 0, overruns: 0 })
    }
    pub fn available(&self) -> usize {
        self.head.wrapping_sub(self.tail)
    }
    fn space(&self) -> usize {
        RING_BYTES - self.available()
    }
    /// Appends what fits; the rest is dropped and counted (the reader is too slow).
    fn push(&mut self, data: &[u8]) -> usize {
        let count = data.len().min(self.space());
        for (offset, &byte) in data[..count].iter().enumerate() {
            unsafe { ((self.base as usize + (self.head + offset) % RING_BYTES) as *mut u8).write(byte) };
        }
        self.head = self.head.wrapping_add(count);
        if count < data.len() {
            self.overruns += 1;
        }
        count
    }
    fn pop(&mut self, out: &mut [u8]) -> usize {
        let count = out.len().min(self.available());
        for (offset, slot) in out[..count].iter_mut().enumerate() {
            *slot = unsafe { ((self.base as usize + (self.tail + offset) % RING_BYTES) as *const u8).read() };
        }
        self.tail = self.tail.wrapping_add(count);
        count
    }
    fn release(&mut self) {
        if self.base != 0 {
            memory::free(self.base, (RING_BYTES / 4096) as u64);
        }
        *self = Ring::empty();
    }
}

struct Stream {
    alt: StreamAlt,
    dci: u32,
    buffers: u64,
    frames: u64,
    /// Slot whose completion is expected next, and the TRB address queued for each slot.
    next: usize,
    trbs: [u64; WINDOW],
    pub ring: Ring,
    /// Playback only: fractional frames per packet, as an accumulator in units of 1/packets-per-second.
    accumulator: u32,
    packets_per_second: u32,
    rate: u32,
    pub packets: u64,
    pub bytes: u64,
    pub errors: u64,
    pub underruns: u64,
}

struct AudioDevice {
    controller: (u8, u8, u8),
    device: Device,
    description: Description,
    /// Rates the device offers (class 1.0: from the descriptor; class 2.0: the clock source's ranges).
    rates: [u32; 8],
    rate_count: usize,
    capture: Option<Stream>,
    playback: Option<Stream>,
}

static mut DEVICE: Option<AudioDevice> = None;

fn audio() -> Option<&'static mut AudioDevice> {
    unsafe { (*core::ptr::addr_of_mut!(DEVICE)).as_mut() }
}

/// Keeps an audio device after enumeration (its slot stays open) and reads its sample rates.
pub fn attach(controller: &mut Controller, device: Device, description: Description, configuration_value: u8) -> bool {
    if audio().is_some() || controller.set_configuration(&device, configuration_value).is_err() {
        return false;
    }
    let mut rates = [0u32; 8];
    let mut rate_count = 0;
    if description.class_version == 2 && description.clock_count > 0 {
        // GET RANGE (request 2) on the clock source's sampling-frequency control: a count, then
        // (minimum, maximum, resolution) triplets of 32-bit values.
        let mut data = [0u8; 2 + 12 * 8];
        let index = (u16::from(description.clock_sources[0]) << 8) | u16::from(description.control_interface);
        if controller.control_transfer(&device, 0xa1, 0x02, 0x0100, index, &mut data).is_ok() {
            let ranges = usize::from(u16::from_le_bytes([data[0], data[1]])).min(8);
            let mut add = |rate: u32, rates: &mut [u32; 8], count: &mut usize| {
                if rate != 0 && *count < rates.len() && !rates[..*count].contains(&rate) {
                    rates[*count] = rate;
                    *count += 1;
                }
            };
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
                if !rates[..rate_count].contains(&rate) && rate_count < rates.len() {
                    rates[rate_count] = rate;
                    rate_count += 1;
                }
            }
        }
    }
    unsafe {
        DEVICE = Some(AudioDevice {
            controller: (controller.pci.bus, controller.pci.device, controller.pci.function),
            device,
            description,
            rates,
            rate_count,
            capture: None,
            playback: None,
        });
    }
    true
}

pub fn present() -> bool {
    audio().is_some()
}

/// The device's alternate setting for a direction, if it has one.
fn find_alt(description: &Description, capture: bool) -> Option<StreamAlt> {
    description.alts[..description.alt_count].iter().copied().find(|alt| (alt.endpoint & 0x80 != 0) == capture)
}

/// Selects the sample rate: class 1.0 sets it on the streaming endpoint (SET_CUR, 3 bytes), class 2.0
/// on the clock source (SET_CUR, 4 bytes).
fn set_rate(controller: &mut Controller, device: &Device, description: &Description, alt: &StreamAlt, rate: u32) -> bool {
    let bytes = rate.to_le_bytes();
    if description.class_version == 2 {
        let index = (u16::from(description.clock_sources[0]) << 8) | u16::from(description.control_interface);
        let mut data = bytes;
        controller.control_transfer(device, 0x21, 0x01, 0x0100, index, &mut data).is_ok()
    } else {
        let mut data = [bytes[0], bytes[1], bytes[2]];
        controller.control_transfer(device, 0x22, 0x01, 0x0100, u16::from(alt.endpoint), &mut data).is_ok()
    }
}

fn start_stream(controller: &mut Controller, device: &Device, description: &Description, alt: StreamAlt, rate: u32, capture: bool) -> Result<Stream, &'static str> {
    if alt.max_packet == 0 {
        return Err("the endpoint has no packet size");
    }
    let frames = ((WINDOW * usize::from(alt.max_packet)).div_ceil(4096)) as u64;
    let buffers = memory::allocate_contiguous(frames, Some(1 << 32)).ok_or("no memory for the packet buffers")?;
    let ring = Ring::allocate().ok_or("no memory for the PCM ring")?;
    controller.control_transfer(device, 0x01, 0x0b, u16::from(alt.alt), u16::from(alt.interface), &mut []).map_err(|_| "SET_INTERFACE failed")?;
    if !set_rate(controller, device, description, &alt, rate) {
        crate::println!("audio: the device refused sample rate {rate} (carrying on at whatever it uses).");
    }
    let dci = controller.configure_isoc_endpoint(device, alt.endpoint, alt.max_packet, alt.interval).map_err(|_| "Configure Endpoint failed")?;
    let interval_shift = u32::from(alt.interval.clamp(1, 16)) - 1;
    let packets_per_second = if device.speed >= 3 { 8000 >> interval_shift } else { 1000 >> interval_shift };
    let mut stream = Stream {
        alt,
        dci,
        buffers,
        frames,
        next: 0,
        trbs: [0; WINDOW],
        ring,
        accumulator: 0,
        packets_per_second: packets_per_second.max(1),
        rate,
        packets: 0,
        bytes: 0,
        errors: 0,
        underruns: 0,
    };
    for slot in 0..WINDOW {
        queue_slot(controller, device.slot, &mut stream, slot, capture);
    }
    controller.start_endpoint(device.slot, dci);
    Ok(stream)
}

/// Fills (playback) or just aims (capture) packet slot `slot` and queues its transfer.
fn queue_slot(controller: &mut Controller, xhci_slot: u8, stream: &mut Stream, slot: usize, capture: bool) {
    let address = stream.buffers + (slot * usize::from(stream.alt.max_packet)) as u64;
    let length = if capture {
        u32::from(stream.alt.max_packet)
    } else {
        // This packet's frame count: rate / packets-per-second, with the remainder carried over so
        // 44.1 kHz comes out as 44 frames per packet with a 45 once in ten.
        stream.accumulator += stream.rate;
        let frames = stream.accumulator / stream.packets_per_second;
        stream.accumulator %= stream.packets_per_second;
        let frame_bytes = usize::from(stream.alt.channels) * usize::from(stream.alt.subslot_bytes);
        let wanted = (frames as usize * frame_bytes).min(usize::from(stream.alt.max_packet));
        let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, wanted) };
        let got = stream.ring.pop(buffer);
        if got < wanted {
            buffer[got..].fill(0);
            stream.underruns += 1;
        }
        wanted as u32
    };
    stream.trbs[slot] = controller.queue_isoc(xhci_slot, stream.dci, address, length);
}

/// Called with every transfer event the xHCI driver has no one waiting for; returns true if it was
/// an audio stream's.
pub fn background_event(controller: &mut Controller, event: &xhci::Trb) -> bool {
    let Some(device) = audio() else { return false };
    if !event.is_transfer() || event.slot_number() != device.device.slot {
        return false;
    }
    if (controller.pci.bus, controller.pci.device, controller.pci.function) != device.controller {
        return false;
    }
    let slot = device.device.slot;
    for (stream, capture) in [(device.capture.as_mut(), true), (device.playback.as_mut(), false)] {
        let Some(stream) = stream else { continue };
        if event.endpoint_index() != stream.dci {
            continue;
        }
        // Completions arrive in queue order; if one was missed, resynchronise on the TRB address.
        let slot_index = if stream.trbs[stream.next] == event.trb_address() {
            stream.next
        } else {
            stream.trbs.iter().position(|&address| address == event.trb_address()).unwrap_or(stream.next)
        };
        stream.next = (slot_index + 1) % WINDOW;
        stream.packets += 1;
        let ok = matches!(event.completion(), xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT);
        if !ok {
            stream.errors += 1;
        }
        if capture && ok {
            let address = stream.buffers + (slot_index * usize::from(stream.alt.max_packet)) as u64;
            let length = usize::from(stream.alt.max_packet).saturating_sub(event.residual() as usize);
            let data = unsafe { core::slice::from_raw_parts(address as *const u8, length) };
            stream.bytes += length as u64;
            stream.ring.push(data);
        } else if !capture {
            stream.bytes += u64::from(stream.alt.max_packet.min(1)); // counted below by packets; bytes unused for playback
        }
        queue_slot(controller, slot, stream, slot_index, capture);
        controller.start_endpoint(slot, stream.dci);
        return true;
    }
    false
}

/// Starts capture and/or playback at `rate`. Returns a reason on failure.
pub fn start(controller: &mut Controller, rate: u32, want_capture: bool, want_playback: bool) -> Result<(), &'static str> {
    let Some(device) = audio() else { return Err("no USB audio device") };
    if device.capture.is_some() || device.playback.is_some() {
        return Err("already running");
    }
    if device.rate_count > 0 && !device.rates[..device.rate_count].contains(&rate) {
        return Err("the device does not offer that sample rate");
    }
    let dev = device.device;
    if want_capture {
        let alt = find_alt(&device.description, true).ok_or("the device has no capture stream")?;
        device.capture = Some(start_stream(controller, &dev, &device.description, alt, rate, true)?);
    }
    if want_playback {
        let alt = find_alt(&device.description, false).ok_or("the device has no playback stream")?;
        device.playback = Some(start_stream(controller, &dev, &device.description, alt, rate, false)?);
    }
    Ok(())
}

fn stop_stream(controller: &mut Controller, device: &Device, stream: &mut Stream) {
    controller.stop_endpoint(device.slot, stream.dci);
    controller.drop_endpoint(device, stream.dci);
    let _ = controller.control_transfer(device, 0x01, 0x0b, 0, u16::from(stream.alt.interface), &mut []); // back to alternate setting 0
    memory::free(stream.buffers, stream.frames);
    stream.ring.release();
}

pub fn stop(controller: &mut Controller) {
    let Some(device) = audio() else { return };
    let dev = device.device;
    if let Some(mut stream) = device.capture.take() {
        stop_stream(controller, &dev, &mut stream);
    }
    if let Some(mut stream) = device.playback.take() {
        stop_stream(controller, &dev, &mut stream);
    }
}

/// The xHCI controller the audio device is on.
pub fn controller() -> Option<&'static mut Controller> {
    let identity = audio()?.controller;
    (0..xhci::count()).find_map(|index| xhci::controller(index).filter(|c| (c.pci.bus, c.pci.device, c.pci.function) == identity))
}

/// Identity of the controller the audio device is on, to find it again.
pub fn controller_identity() -> Option<(u8, u8, u8)> {
    audio().map(|device| device.controller)
}

/// Copies captured bytes (interleaved frames) out of the PCM ring.
pub fn read(out: &mut [u8]) -> usize {
    audio().and_then(|device| device.capture.as_mut()).map_or(0, |stream| stream.ring.pop(out))
}

/// Queues playback bytes (interleaved frames); returns how many fitted.
pub fn write(data: &[u8]) -> usize {
    audio().and_then(|device| device.playback.as_mut()).map_or(0, |stream| stream.ring.push(data))
}

/// `audio` with no arguments: what the device offers and how a running stream is doing.
pub fn status() {
    let Some(device) = audio() else { return crate::println!("No USB audio device.") };
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
                stream.rate, stream.alt.channels, stream.alt.bits, stream.packets, stream.bytes, stream.errors, stream.underruns, stream.ring.available(), stream.ring.overruns
            ),
            None => crate::println!("  {name}: stopped"),
        }
    }
}

/// Channel layout of the capture stream: (channels, bytes per sample slot).
pub fn capture_format() -> Option<(u8, u8)> {
    audio().and_then(|device| device.capture.as_ref()).map(|stream| (stream.alt.channels, stream.alt.subslot_bytes))
}

/// Channel layout of the playback stream: (channels, bytes per sample slot).
pub fn playback_format() -> Option<(u8, u8)> {
    audio().and_then(|device| device.playback.as_ref()).map(|stream| (stream.alt.channels, stream.alt.subslot_bytes))
}

/// Bytes of room in the playback PCM ring.
pub fn playback_space() -> usize {
    audio().and_then(|device| device.playback.as_ref()).map_or(0, |stream| stream.ring.space())
}
