//! USB-MIDI (Audio class 1, subclass 3, "MIDIStreaming") interfaces on xHCI: class-compliant MIDI cables and controllers.
//!
//! What the device looks like (USB-MIDI 1.0): one interface with a bulk IN and a bulk OUT endpoint. Everything on them
//! is carried in 4-byte "USB-MIDI event packets": byte 0 is (cable number << 4) | code index number (CIN), then up to
//! three MIDI bytes. The CIN says how many of them are real (`packet_length`). A cheap full-speed cable has 8-byte
//! bulk packets, so one USB packet holds two events at most; a device ends a transfer with a short packet whenever it
//! has nothing more, so the IN transfer is queued one max-packet at a time: each packet completes it and is re-queued.
//!
//! IN events arrive in `background_event` (called wherever the xHCI event ring is drained, like the keyboards') and are
//! kept in a small ring that `monitor` and the tape calls to come read with `take`. OUT is one blocking bulk transfer per
//! message (`send`). Only xHCI devices are handled (a MIDI device on a hub works as well: it is found by `usb_hid::probe_device`).
//!
//! Without the hardware to hand this was written from the USB-MIDI 1.0 class specification and a `lsusb -v` dump of a
//! 2381:0100 cable; the QEMU `usb-host` passthrough is the only way to run it without the real device on the machine.

use crate::memory;
use crate::xhci::{self, Controller, Device, Trb};

const MAX_DEVICES: usize = 4;
const EVENTS: usize = 256;

/// The endpoints of the MIDIStreaming interface: (interface number, IN address, IN max packet, OUT address, OUT max packet).
#[allow(dead_code)]
pub struct Endpoints {
    pub interface: u8,
    pub in_endpoint: u8,
    pub in_max_packet: u16,
    pub out_endpoint: u8,
    pub out_max_packet: u16,
}

#[derive(Clone, Copy)]
struct Midi {
    bus: u8,
    device_number: u8,
    function: u8,
    device: Device,
    vendor: u16,
    product: u16,
    in_dci: u32,
    in_max_packet: u16,
    out_endpoint: u8,
    out_max_packet: u16,
    /// A page: the IN packet lands in the first 64 bytes, the OUT packet is built after it.
    buffer: u64,
    received: u64,
    sent: u64,
}

static mut DEVICES: [Option<Midi>; MAX_DEVICES] = [None; MAX_DEVICES];
/// (device index, cable, three MIDI bytes of an event packet's payload, how many are real).
static mut QUEUE: [[u8; 6]; EVENTS] = [[0; 6]; EVENTS];
static mut HEAD: usize = 0;
static mut TAIL: usize = 0;
static mut DROPPED: u64 = 0;

/// How many MIDI bytes follow the header byte of an event packet with this code index number (0 for the reserved ones).
pub fn packet_length(cin: u8) -> usize {
    match cin & 0x0f {
        0x2 | 0x6 | 0xc | 0xd => 2,
        0x3 | 0x4 | 0x7 | 0x8 | 0x9 | 0xa | 0xb | 0xe => 3,
        0x5 | 0xf => 1,
        _ => 0,
    }
}

/// Walks a configuration descriptor for a MIDIStreaming interface (class 1, subclass 3) with a bulk IN and a bulk OUT endpoint.
pub fn find_endpoints(configuration: &[u8]) -> Option<Endpoints> {
    let mut at = 0;
    let mut interface = None::<u8>;
    let (mut input, mut output) = (None, None);
    while at + 2 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            break;
        }
        let descriptor = &configuration[at..at + length];
        match descriptor[1] {
            4 if length >= 9 => {
                if interface.is_some() && input.is_some() && output.is_some() {
                    break;
                }
                // Only the first alternate setting: it is the one every cable uses.
                interface = (descriptor[5] == 1 && descriptor[6] == 3 && descriptor[3] == 0).then_some(descriptor[2]);
                input = None;
                output = None;
            }
            5 if length >= 7 && interface.is_some() && descriptor[3] & 3 == 2 => {
                let packet = (u8::from_le_bytes([descriptor[2]]), u16::from_le_bytes([descriptor[4], descriptor[5]]) & 0x7ff);
                if packet.0 & 0x80 != 0 {
                    input.get_or_insert(packet);
                } else {
                    output.get_or_insert(packet);
                }
            }
            _ => {}
        }
        at += length;
    }
    let (input, output) = (input?, output?);
    Some(Endpoints { interface: interface?, in_endpoint: input.0, in_max_packet: input.1, out_endpoint: output.0, out_max_packet: output.1 })
}

/// Configures the endpoints of an addressed, configured device and starts listening. Returns false if it could not.
pub fn attach(controller: &mut Controller, device: Device, endpoints: &Endpoints, vendor: u16, product: u16) -> bool {
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(DEVICES) };
    let Some(entry) = entries.iter_mut().find(|entry| entry.is_none()) else { return false };
    let Some(buffer) = memory::allocate_contiguous(1, Some(1 << 32)) else { return false };
    if controller.configure_bulk_endpoints(&device, endpoints.in_endpoint, endpoints.in_max_packet, 0, endpoints.out_endpoint, endpoints.out_max_packet, 0).is_err() {
        memory::free(buffer, 1);
        return false;
    }
    let in_dci = u32::from(endpoints.in_endpoint & 0x0f) * 2 + 1;
    *entry = Some(Midi {
        bus: controller.pci.bus,
        device_number: controller.pci.device,
        function: controller.pci.function,
        device,
        vendor,
        product,
        in_dci,
        in_max_packet: endpoints.in_max_packet.clamp(4, 64),
        out_endpoint: endpoints.out_endpoint,
        out_max_packet: endpoints.out_max_packet.max(4),
        buffer,
        received: 0,
        sent: 0,
    });
    let length = u32::from(endpoints.in_max_packet.clamp(4, 64));
    controller.queue_interrupt_in(device.slot, in_dci, buffer, length); // a Normal TRB with IOC and ISP: right for a bulk IN as well
    crate::println!(
        "USB MIDI {vendor:04x}:{product:04x} on port {} of xHCI {:02x}:{:02x}.{} ({}): 1 input, 1 output, {}-byte packets. `midi monitor` shows what it sends.",
        device.path(),
        controller.pci.bus,
        controller.pci.device,
        controller.pci.function,
        xhci::speed_name(device.speed),
        endpoints.in_max_packet
    );
    true
}

fn queue_push(index: usize, cable: u8, bytes: &[u8]) {
    unsafe {
        let next = (HEAD + 1) % EVENTS;
        if next == TAIL {
            DROPPED += 1;
            return;
        }
        let mut entry = [index as u8, cable, 0, 0, 0, bytes.len() as u8];
        entry[2..2 + bytes.len()].copy_from_slice(bytes);
        (*core::ptr::addr_of_mut!(QUEUE))[HEAD] = entry;
        HEAD = next;
    }
}

/// An IN transfer finished: split the packet into events, queue them and listen again.
pub fn background_event(controller: &mut Controller, event: &Trb) -> bool {
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(DEVICES) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    let Some((index, midi)) = entries.iter_mut().enumerate().find_map(|(index, entry)| {
        let midi = entry.as_mut()?;
        ((midi.bus, midi.device_number, midi.function) == identity && midi.device.slot == event.slot_number() && midi.in_dci == event.endpoint_index()).then_some((index, midi))
    }) else {
        return false;
    };
    let (slot, dci, buffer, length) = (midi.device.slot, midi.in_dci, midi.buffer, u32::from(midi.in_max_packet));
    match event.completion() {
        xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT => {
            let got = length.saturating_sub(event.residual()).min(64) as usize;
            let packet: [u8; 64] = unsafe { core::ptr::read_volatile(buffer as *const [u8; 64]) };
            for event in packet[..got].chunks_exact(4) {
                let count = packet_length(event[0]);
                if count > 0 {
                    midi.received += 1;
                    queue_push(index, event[0] >> 4, &event[1..1 + count]);
                }
            }
        }
        _ => controller.reset_stalled_endpoint(slot, dci),
    }
    controller.queue_interrupt_in(slot, dci, buffer, length);
    true
}

/// Forgets the devices in the slots of `port` (only the one at `route` below it if given); call before the slots go.
pub fn release(controller: &mut Controller, port: u8, route: Option<u32>) {
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(DEVICES) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    for entry in entries.iter_mut() {
        let Some(midi) = *entry else { continue };
        let there = controller.slot_location(midi.device.slot).is_some_and(|(p, r)| p == port && route.is_none_or(|wanted| wanted == r));
        if (midi.bus, midi.device_number, midi.function) == identity && there {
            *entry = None;
            // The controller stops writing into the buffer when the slot is released, which the caller does next; the
            // page is a few bytes of a leak at worst until then, so it is not handed back here.
        }
    }
}

pub fn count() -> usize {
    unsafe { (*core::ptr::addr_of!(DEVICES)).iter().flatten().count() }
}

/// The next received message: (device, cable, bytes, how many).
pub fn take() -> Option<(usize, u8, [u8; 3], usize)> {
    unsafe {
        if TAIL == HEAD {
            return None;
        }
        let entry = (*core::ptr::addr_of!(QUEUE))[TAIL];
        TAIL = (TAIL + 1) % EVENTS;
        Some((usize::from(entry[0]), entry[1], [entry[2], entry[3], entry[4]], usize::from(entry[5])))
    }
}

/// Sends one MIDI message (a channel or system message of 1-3 bytes; no System Exclusive) on cable 0 of device `index`.
pub fn send(index: usize, message: &[u8]) -> Result<(), &'static str> {
    let Some(first) = message.first().copied() else { return Err("empty message") };
    let (cin, length) = match first {
        0x80..=0xbf | 0xe0..=0xef => (first >> 4, 3),
        0xc0..=0xdf => (first >> 4, 2),
        0xf1 | 0xf3 => (0x2, 2),
        0xf2 => (0x3, 3),
        0xf6 | 0xf8..=0xff => (if first == 0xf6 { 0x5 } else { 0xf }, 1),
        _ => return Err("not a message this command can send (a status byte of 0x80-0xef, or a system message but not SysEx)"),
    };
    if message.len() != length {
        return Err("wrong number of bytes for that status byte");
    }
    let entries = unsafe { &mut *core::ptr::addr_of_mut!(DEVICES) };
    let Some(midi) = entries.get_mut(index).and_then(|entry| entry.as_mut()) else { return Err("no such MIDI device") };
    let packet = midi.buffer + 64;
    unsafe {
        let bytes = packet as *mut u8;
        bytes.write_volatile(cin);
        for slot in 1..4 {
            bytes.add(slot).write_volatile(*message.get(slot - 1).unwrap_or(&0));
        }
    }
    let identity = (midi.bus, midi.device_number, midi.function);
    let controller = (0..xhci::count())
        .filter_map(xhci::controller)
        .find(|controller| (controller.pci.bus, controller.pci.device, controller.pci.function) == identity)
        .ok_or("the controller is gone")?;
    controller.bulk_transfer(&midi.device, midi.out_endpoint, packet, 4, midi.out_max_packet).map_err(|_| "the transfer failed")?;
    midi.sent += 1;
    Ok(())
}

/// A message as text: its kind and channel (1-16), and the data bytes (a note as its number and name).
pub struct Describe<'a>(pub &'a [u8]);

impl core::fmt::Display for Describe<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let bytes = self.0;
        let status = bytes[0];
        for byte in bytes {
            write!(f, "{byte:02x} ")?;
        }
        let channel = (status & 0x0f) + 1;
        let (first, second) = (bytes.get(1).copied().unwrap_or(0), bytes.get(2).copied().unwrap_or(0));
        match status {
            0x80..=0x9f => {
                const NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
                let on = status >= 0x90 && second != 0;
                write!(f, "{} ch {channel} {}{} velocity {second}", if on { "note on " } else { "note off" }, NAMES[usize::from(first % 12)], i32::from(first / 12) - 1)
            }
            0xa0..=0xaf => write!(f, "aftertouch ch {channel} note {first} pressure {second}"),
            0xb0..=0xbf => write!(f, "control ch {channel} controller {first} value {second}"),
            0xc0..=0xcf => write!(f, "program ch {channel} {first}"),
            0xd0..=0xdf => write!(f, "channel pressure ch {channel} {first}"),
            0xe0..=0xef => write!(f, "pitch bend ch {channel} {}", i32::from(first) | (i32::from(second) << 7)),
            0xf8 => write!(f, "clock"),
            0xfa => write!(f, "start"),
            0xfb => write!(f, "continue"),
            0xfc => write!(f, "stop"),
            0xfe => write!(f, "active sensing"),
            _ => write!(f, "system message"),
        }
    }
}

pub fn describe_devices(print: impl Fn(core::fmt::Arguments)) {
    for (number, midi) in unsafe { (*core::ptr::addr_of!(DEVICES)).iter().enumerate().filter_map(|(number, entry)| Some((number, entry.as_ref()?))) } {
        print(format_args!(
            "MIDI {}: {:04x}:{:04x} on port {}, {} event(s) in, {} sent",
            number + 1,
            midi.vendor,
            midi.product,
            midi.device.path(),
            midi.received,
            midi.sent
        ));
    }
    let dropped = unsafe { DROPPED };
    if dropped > 0 {
        print(format_args!("MIDI: {dropped} event(s) dropped (nobody was reading)"));
    }
}
