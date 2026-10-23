//! USB boot keyboards over xHCI for the aarch64 port: enumerate the root ports, find a HID boot keyboard, arm its
//! interrupt-IN endpoint and turn reports into bytes for the shell. The x86 `usb_hid.rs` does this and much more (hubs,
//! EHCI/OHCI, disks, audio, MIDI, serial) and is too entangled with those to carry over yet; this is the keyboard part
//! only, on the same `xhci::Controller` calls. Polling only: `poll()` drains the event rings from the shell loop.
//! It is re-exported as `crate::usb_hid` so the shared xhci.rs finds its `background_event` hook.

use crate::memory;
use crate::xhci::{self, Controller, Trb};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
static SWEDISH: AtomicBool = AtomicBool::new(false);
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

/// The layout a `keymap` word names (the x86 `keyboard::layout_named`).
pub fn layout_named(name: &str) -> Option<Layout> {
    match name {
        "us" => Some(Layout::Us),
        "sv" | "se" | "fi" => Some(Layout::Swedish),
        _ => None,
    }
}

/// Chooses what the keys type; US until told otherwise (`keymap` command, `settings` file).
pub fn set_layout(layout: Layout) {
    SWEDISH.store(layout == Layout::Swedish, Ordering::Relaxed);
}

pub fn layout() -> Layout {
    if SWEDISH.load(Ordering::Relaxed) { Layout::Swedish } else { Layout::Us }
}

/// The next typed byte, if one is waiting.
pub fn keyboard_count() -> usize {
    unsafe { (*core::ptr::addr_of!(KEYBOARDS)).iter().flatten().count() }
}

/// The Pi's keyboards keep no report counter.
pub fn reports_seen() -> u64 {
    0
}

pub fn pop() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { (*core::ptr::addr_of!(KEYS))[tail] };
    TAIL.store((tail + 1) % QUEUE, Ordering::Release);
    Some(byte)
}

static mut STEP: &str = "";

/// Names the enumeration step in progress, for the "attempt failed at ..." messages (`usb_hub` uses it too).
pub fn set_step(step: &'static str) {
    unsafe { STEP = step };
}

pub fn step() -> &'static str {
    unsafe { STEP }
}

/// Brings up every xHCI controller and starts any boot keyboard on its connected root ports and on the hubs behind them.
/// Returns how many keyboards.
pub fn init() -> usize {
    let controllers = xhci::init_all();
    crate::println!("USB: {controllers} xHCI controller(s).");
    probe_all()
}

/// Looks at every connected root port that is not in use yet, and at every known hub's ports. Returns how many keyboards
/// were added (the `usb` shell command runs this again after something was plugged in).
pub fn probe_all() -> usize {
    let mut found = 0;
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        for port in 1..=controller.ports {
            if controller.slot_in_use_for_port(port) {
                // A device enumerated here earlier: if the port says it went away (or was replugged), drop its slot, mark its
                // disk stale (a returning stick takes its old number back) and look at the port afresh.
                if controller.port_status(port) & 1 != 0 && !controller.port_connect_changed(port) {
                    continue;
                }
                crate::usb_serial::xhci_departed(index, port);
                crate::usb_storage::mark_xhci_port_stale(index, port);
                crate::usb_hub::forget_hubs_on_port(index, port);
                release_port(controller, port, None);
                crate::println!("USB: xHCI {index} port {port}: its device was unplugged or replugged; looking at the port again.");
            }
            if controller.port_status(port) & 1 == 0 {
                continue;
            }
            for attempt in 1..=3 {
                set_step("open the port (reset, enable slot, address device)");
                match probe_port(index, controller, port) {
                    Ok(true) => {
                        found += 1;
                        break;
                    }
                    Ok(false) => break,
                    Err(error) => {
                        crate::println!("USB: xHCI {index} port {port}, attempt {attempt}/3 failed at \"{}\": {error:?}.", step());
                        controller.release_port_slots(port);
                        crate::timer::wait_ticks(15);
                    }
                }
            }
        }
        found += crate::usb_hub::rescan(index, controller);
    }
    found
}

/// Drops the slots in a port (or, with a route, one device behind a hub) and the keyboard entries that used them. The
/// controller stops writing reports (the slots go first) before the report buffer is handed back.
pub fn release_port(controller: &mut Controller, port: u8, route: Option<u32>) {
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    let mut buffers = [0u64; MAX_KEYBOARDS];
    for (entry, freed) in keyboards.iter_mut().zip(buffers.iter_mut()) {
        let Some(keyboard) = *entry else { continue };
        let there = controller.slot_location(keyboard.slot).is_some_and(|(p, r)| p == port && route.is_none_or(|wanted| wanted == r));
        if (keyboard.bus, keyboard.device, keyboard.function) == identity && there {
            *entry = None;
            *freed = keyboard.buffer;
        }
    }
    crate::usb_midi::release(controller, port, route);
    match route {
        Some(route) => controller.release_route_slots(port, route),
        None => controller.release_port_slots(port),
    }
    for buffer in buffers.into_iter().filter(|&buffer| buffer != 0) {
        memory::free(buffer, 1);
    }
}

fn probe_port(controller_index: usize, controller: &mut Controller, port: u8) -> Result<bool, xhci::Error> {
    let device = controller.open_port(port)?;
    probe_device(controller_index, controller, device)
}

/// Reads an addressed device's descriptors: a hub is enumerated (its children come back here), a boot keyboard is started,
/// anything else is ignored for now.
pub fn probe_device(controller_index: usize, controller: &mut Controller, mut device: xhci::Device) -> Result<bool, xhci::Error> {
    set_step("read the device descriptor");
    let mut descriptor = [0u8; 18];
    controller.get_descriptor(&device, 1, 0, &mut descriptor[..8])?;
    // SuperSpeed gives bMaxPacketSize0 as an exponent (always 9 = 512); the other speeds as the size itself.
    let max_packet = if device.speed >= 4 { 512 } else { u16::from(descriptor[7]).max(8) };
    controller.set_control_max_packet(&mut device, max_packet)?;
    controller.get_descriptor(&device, 1, 0, &mut descriptor)?;
    let (vendor, product) = (u16::from_le_bytes([descriptor[8], descriptor[9]]), u16::from_le_bytes([descriptor[10], descriptor[11]]));
    let port = device.path();
    set_step("read the configuration descriptor");
    let mut configuration = [0u8; 512];
    controller.get_descriptor(&device, 2, 0, &mut configuration[..9])?;
    if descriptor[4] == 9 {
        crate::println!("USB: port {port}: a hub ({vendor:04x}:{product:04x}, {}).", xhci::speed_name(device.speed));
        return crate::usb_hub::enumerate(controller_index, controller, device, configuration[5]);
    }
    let total = usize::from(u16::from_le_bytes([configuration[2], configuration[3]])).clamp(9, configuration.len());
    controller.get_descriptor(&device, 2, 0, &mut configuration[..total])?;
    let Some((interface, endpoint, max_packet_in, interval)) = find_boot_keyboard(&configuration[..total]) else {
        // A USB stick (Mass Storage, Bulk-Only) becomes a disk.
        if let Some(endpoints) = crate::usb_storage::find_endpoints(&configuration[..total]) {
            controller.set_configuration(&device, configuration[5])?;
            return Ok(match crate::usb_storage::attach_xhci(controller_index, controller, device, endpoints) {
                Some(disk) => {
                    crate::println!("USB storage {vendor:04x}:{product:04x} on port {port} ({}): {}.", xhci::speed_name(device.speed), crate::storage::DiskNumber(disk));
                    true
                }
                None => {
                    controller.release_slot(device.slot);
                    false
                }
            });
        }
        // Audio goes first: attaching it selects the configuration, which would reset the bulk endpoints MIDI configured.
        let midi = crate::usb_midi::find_endpoints(&configuration[..total]);
        let audio = crate::usb_audio::is_audio(&configuration[..total]);
        if midi.is_some() || audio {
            let mut attached = false;
            if audio {
                let description = crate::usb_audio::parse(&configuration[..total]);
                crate::usb_audio::report(&description, &device.path(), vendor, product, xhci::speed_name(device.speed));
                attached = crate::usb_audio::attach(controller, device, description, configuration[5]);
            }
            if let Some(endpoints) = midi {
                if !attached {
                    controller.set_configuration(&device, configuration[5])?;
                }
                attached |= crate::usb_midi::attach(controller, device, &endpoints, vendor, product);
            }
            if attached {
                return Ok(true);
            }
            controller.release_slot(device.slot);
            return Ok(false);
        }
        // A USB serial adapter (PL2303, TI): bulk endpoints carry raw bytes, the `serial_*` tape calls reach them.
        match crate::usb_serial::attach_xhci(controller_index, controller, device, vendor, product, descriptor[17]) {
            crate::usb_serial::Outcome::Attached => {
                crate::println!("USB serial {vendor:04x}:{product:04x} on port {port} ({}): ready (see `usb`).", xhci::speed_name(device.speed));
                return Ok(true);
            }
            crate::usb_serial::Outcome::Reprobe | crate::usb_serial::Outcome::Retry => {
                controller.release_slot(device.slot); // enumerated afresh on the next pass
                return Ok(false);
            }
            crate::usb_serial::Outcome::Unclaimed => {}
        }
        crate::usb_pointer::describe(controller, &device, &configuration[..total], vendor, product);
        crate::println!("USB: port {port}: {vendor:04x}:{product:04x} (class {:#04x}, {}): not a keyboard or disk, ignored for now.", descriptor[4], xhci::speed_name(device.speed));
        controller.release_slot(device.slot);
        return Ok(false);
    };
    set_step("start the keyboard");
    controller.set_configuration(&device, configuration[5])?;
    controller.control_transfer(&device, 0x21, 0x0b, 0, u16::from(interface), &mut [])?; // SET_PROTOCOL(boot)
    let _ = controller.control_transfer(&device, 0x21, 0x0a, 0, u16::from(interface), &mut []); // SET_IDLE(0); may stall
    let dci = controller.configure_interrupt_in(&device, endpoint, max_packet_in, interval)?;
    let buffer = memory::allocate_contiguous(1, Some(1 << 32)).ok_or(xhci::Error::NoMemory)?;
    let slot = unsafe { (*core::ptr::addr_of_mut!(KEYBOARDS)).iter_mut().find(|entry| entry.is_none()) };
    let Some(slot) = slot else {
        controller.release_slot(device.slot);
        memory::free(buffer, 1);
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
    if !event.is_transfer() || crate::usb_midi::background_event(controller, event) || crate::usb_serial::background_event(controller, event) {
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
        } else if let Some(byte) = hid_key(usage, report[0], keyboard.caps, if SWEDISH.load(Ordering::Relaxed) { Layout::Swedish } else { Layout::Us }) {
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
