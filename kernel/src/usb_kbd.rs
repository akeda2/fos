//! USB boot keyboards over xHCI for the aarch64 port: enumerate the root ports, find a HID boot keyboard, arm its
//! interrupt-IN endpoint and turn reports into bytes for the shell. The x86 `usb_hid.rs` does this and much more (hubs,
//! EHCI/OHCI, disks, audio, MIDI, serial) and is too entangled with those to carry over yet; this is the keyboard part
//! only, on the same `xhci::Controller` calls. Polling only: `poll()` drains the event rings from the shell loop.
//! It is re-exported as `crate::usb_hid` so the shared xhci.rs finds its `background_event` hook.

use crate::memory;
use crate::xhci::{self, Controller, Trb};
use core::sync::atomic::{AtomicUsize, Ordering};
use fosfor_abi::keymap::{hid_key, Layout};

const MAX_KEYBOARDS: usize = 4;
const QUEUE: usize = 64;

#[derive(Clone, Copy)]
struct Keyboard {
    bus: u8,
    device: u8,
    function: u8,
    slot: u8,
    dci: u32,
    buffer: u64,
    previous: [u8; 8],
    caps: bool,
}

static mut KEYBOARDS: [Option<Keyboard>; MAX_KEYBOARDS] = [None; MAX_KEYBOARDS];
static mut KEYS: [u8; QUEUE] = [0; QUEUE];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

fn push(byte: u8) {
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % QUEUE;
    if next != TAIL.load(Ordering::Acquire) {
        unsafe { (*core::ptr::addr_of_mut!(KEYS))[head] = byte };
        HEAD.store(next, Ordering::Release);
    }
}

/// The next typed byte, if one is waiting.
pub fn pop() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { (*core::ptr::addr_of!(KEYS))[tail] };
    TAIL.store((tail + 1) % QUEUE, Ordering::Release);
    Some(byte)
}

/// Brings up every xHCI controller and starts any boot keyboard on its connected root ports. Returns how many keyboards.
pub fn init() -> usize {
    let controllers = xhci::init_all();
    crate::println!("USB: {controllers} xHCI controller(s).");
    let mut found = 0;
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        for port in 1..=controller.ports {
            if controller.port_status(port) & 1 == 0 {
                continue;
            }
            for attempt in 1..=3 {
                match probe_port(controller, port) {
                    Ok(true) => {
                        found += 1;
                        break;
                    }
                    Ok(false) => break,
                    Err(error) => {
                        crate::println!("USB: xHCI {index} port {port}, attempt {attempt}/3 failed: {error:?}.");
                        controller.release_port_slots(port);
                        crate::timer::wait_ticks(15);
                    }
                }
            }
        }
    }
    found
}

fn probe_port(controller: &mut Controller, port: u8) -> Result<bool, xhci::Error> {
    let mut device = controller.open_port(port)?;
    let mut descriptor = [0u8; 18];
    controller.get_descriptor(&device, 1, 0, &mut descriptor[..8])?;
    // SuperSpeed gives bMaxPacketSize0 as an exponent (always 9 = 512); the other speeds as the size itself.
    let max_packet = if device.speed >= 4 { 512 } else { u16::from(descriptor[7]).max(8) };
    controller.set_control_max_packet(&mut device, max_packet)?;
    controller.get_descriptor(&device, 1, 0, &mut descriptor)?;
    let (vendor, product) = (u16::from_le_bytes([descriptor[8], descriptor[9]]), u16::from_le_bytes([descriptor[10], descriptor[11]]));
    let mut configuration = [0u8; 512];
    controller.get_descriptor(&device, 2, 0, &mut configuration[..9])?;
    let total = usize::from(u16::from_le_bytes([configuration[2], configuration[3]])).clamp(9, configuration.len());
    controller.get_descriptor(&device, 2, 0, &mut configuration[..total])?;
    let Some((interface, endpoint, max_packet_in, interval)) = find_boot_keyboard(&configuration[..total]) else {
        crate::println!("USB: port {port}: {vendor:04x}:{product:04x} (class {:#04x}, {}): not a keyboard, ignored for now.", descriptor[4], xhci::speed_name(device.speed));
        controller.release_slot(device.slot);
        return Ok(false);
    };
    controller.set_configuration(&device, configuration[5])?;
    controller.control_transfer(&device, 0x21, 0x0b, 0, u16::from(interface), &mut [])?; // SET_PROTOCOL(boot)
    let _ = controller.control_transfer(&device, 0x21, 0x0a, 0, u16::from(interface), &mut []); // SET_IDLE(0); may stall
    let dci = controller.configure_interrupt_in(&device, endpoint, max_packet_in, interval)?;
    let buffer = memory::allocate_contiguous(1, Some(1 << 32)).ok_or(xhci::Error::NoMemory)?;
    let slot = unsafe { (*core::ptr::addr_of_mut!(KEYBOARDS)).iter_mut().find(|entry| entry.is_none()) };
    let Some(slot) = slot else {
        controller.release_slot(device.slot);
        return Ok(false);
    };
    *slot = Some(Keyboard { bus: controller.pci.bus, device: controller.pci.device, function: controller.pci.function, slot: device.slot, dci, buffer, previous: [0; 8], caps: false });
    controller.queue_interrupt_in(device.slot, dci, buffer, 8);
    crate::println!("USB keyboard {vendor:04x}:{product:04x} on port {port} ({}): ready.", xhci::speed_name(device.speed));
    Ok(true)
}

/// A HID boot keyboard interface and its interrupt-IN endpoint: (interface, endpoint address, max packet, bInterval).
fn find_boot_keyboard(descriptors: &[u8]) -> Option<(u8, u8, u16, u8)> {
    let mut at = 0;
    let mut interface = None;
    while at + 2 <= descriptors.len() {
        let length = usize::from(descriptors[at]);
        if length < 2 || at + length > descriptors.len() {
            break;
        }
        match descriptors[at + 1] {
            4 if length >= 9 => interface = (descriptors[at + 5] == 3 && descriptors[at + 6] == 1 && descriptors[at + 7] == 1).then_some(descriptors[at + 2]),
            5 if length >= 7 => {
                if let Some(number) = interface {
                    let (address, attributes) = (descriptors[at + 2], descriptors[at + 3]);
                    if address & 0x80 != 0 && attributes & 3 == 3 {
                        return Some((number, address, u16::from_le_bytes([descriptors[at + 4], descriptors[at + 5]]) & 0x7ff, descriptors[at + 6]));
                    }
                }
            }
            _ => {}
        }
        at += length;
    }
    None
}

/// Handles an event nobody was waiting for: a keyboard's report completing (called by xhci.rs's wait loops and `poll`).
pub fn background_event(controller: &mut Controller, event: &Trb) {
    if !event.is_transfer() {
        return;
    }
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    let Some(keyboard) = keyboards.iter_mut().flatten().find(|k| k.slot == event.slot_number() && k.dci == event.endpoint_index() && (k.bus, k.device, k.function) == identity) else {
        return;
    };
    match event.completion() {
        xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT => {
            let report: [u8; 8] = unsafe { core::ptr::read_volatile(keyboard.buffer as *const [u8; 8]) };
            decode(keyboard, &report);
        }
        _ => controller.reset_stalled_endpoint(keyboard.slot, keyboard.dci),
    }
    controller.queue_interrupt_in(keyboard.slot, keyboard.dci, keyboard.buffer, 8);
}

/// Keys present now that were not in the previous report are new presses.
fn decode(keyboard: &mut Keyboard, report: &[u8; 8]) {
    for &usage in &report[2..8] {
        if usage < 4 || keyboard.previous[2..8].contains(&usage) {
            continue;
        }
        if usage == 0x39 {
            keyboard.caps = !keyboard.caps;
        } else if let Some(byte) = hid_key(usage, report[0], keyboard.caps, Layout::Us) {
            push(byte);
        }
    }
    keyboard.previous = *report;
}

/// Drains every controller's event ring; keyboard reports become bytes for `pop`.
pub fn poll() {
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        while let Some(event) = controller.next_event() {
            background_event(controller, &event);
        }
    }
}
