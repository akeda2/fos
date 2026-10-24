//! USB HID keyboards over xHCI: enumeration and report decoding.
//!
//! For each root port with a device: reset and address it, read its descriptors, and
//! look for a HID *boot keyboard* (interface class 3, subclass 1, protocol 1). Such a
//! keyboard speaks a fixed 8-byte report on an interrupt-IN endpoint: a modifier
//! byte, a reserved byte and up to six pressed key usages. The driver switches it to
//! the boot protocol (so no report descriptor parsing is needed), arms the endpoint
//! and, whenever a report completes, turns newly pressed keys into ASCII for the
//! shell. HID keyboards have no hardware auto-repeat, so holding a key types once.
//!
//! Nothing here can stop the boot: every failure is reported and the port skipped.

use fosfor_abi::keymap::hid_key;

use crate::xhci::{self, Controller, Trb};
use crate::{ehci, ohci, usb_storage};
use crate::{keyboard, memory};

const MAX_KEYBOARDS: usize = 4;

/// Where a keyboard's reports come from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// An xHCI slot's interrupt endpoint; the controller is identified by its PCI address.
    Xhci { bus: u8, device: u8, function: u8, slot: u8, dci: u32, buffer: u64 },
    /// A permanent queue head on an EHCI controller.
    Ehci { controller: usize, index: usize },
    /// A permanent endpoint descriptor on an OHCI controller's periodic list.
    Ohci { controller: usize, index: usize },
}

#[derive(Clone, Copy)]
struct Keyboard {
    source: Source,
    previous: [u8; 8],
    caps: bool,
    reports: u64,
}

static mut KEYBOARDS: [Option<Keyboard>; MAX_KEYBOARDS] = [None; MAX_KEYBOARDS];

pub fn keyboard_count() -> usize {
    unsafe { (*core::ptr::addr_of!(KEYBOARDS)).iter().flatten().count() }
}

pub fn reports_seen() -> u64 {
    unsafe { (*core::ptr::addr_of!(KEYBOARDS)).iter().flatten().map(|keyboard| keyboard.reports).sum() }
}

/// Releases the controller slots of whatever is on `port` (only the device at `route` below it if given) and forgets the
/// keyboards that were in them. Without this a replugged keyboard left its old entry behind: the table filled up after
/// four replugs, `usb` counted ghosts, and the slot number the new device was given matched the old entry's event
/// lookup, so its reports were read from the freed report buffer.
pub fn release_port(controller: &mut Controller, port: u8, route: Option<u32>) {
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    let mut buffers = [0u64; MAX_KEYBOARDS];
    for (entry, buffer) in keyboards.iter_mut().zip(buffers.iter_mut()) {
        let Some(Keyboard { source: Source::Xhci { bus, device, function, slot, buffer: report, .. }, .. }) = *entry else { continue };
        let there = controller.slot_location(slot).is_some_and(|(p, r)| p == port && route.is_none_or(|wanted| wanted == r));
        if (bus, device, function) == identity && there {
            *entry = None;
            *buffer = report;
        }
    }
    crate::usb_midi::release(controller, port, route);
    crate::usb_pointer::release(controller, port, route);
    // The slots go first: the controller must stop writing reports before their buffer is handed back.
    match route {
        Some(route) => controller.release_route_slots(port, route),
        None => controller.release_port_slots(port),
    }
    for buffer in buffers.into_iter().filter(|&buffer| buffer != 0) {
        memory::free(buffer, 1);
    }
}

/// Looks at every connected root port that is not already in use and starts any boot
/// keyboard found. Returns how many keyboards were added.
pub fn probe_all() -> usize {
    let added = probe_all_once();
    // A device that took firmware (a USB serial adapter) is reset and enumerated once more; one extra pass, so no loop.
    if crate::usb_serial::take_reprobe() {
        crate::timer::wait_ticks(30);
        return added + probe_all_once();
    }
    added
}

/// Root ports (bit = port number) whose device was enumerated and found unused (a Bluetooth controller, say). A rescan leaves
/// them alone until the port reports a change: resetting and re-addressing such a device on every `usb rescan` made some of
/// them stop answering (ports 7 and 8 of the laptop1 xHCI timed out three times each).
const IDLE_CONTROLLERS: usize = 4;
static mut IDLE_PORTS: [u64; IDLE_CONTROLLERS] = [0; IDLE_CONTROLLERS];

fn probe_all_once() -> usize {
    let mut added = 0;
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        for port in 1..=controller.ports {
            if controller.slot_in_use_for_port(port) {
                // A device we enumerated here earlier. If the port says it went away (or went away and came back), drop its
                // slot and look at the port afresh; a disk that returns is recognised in usb_storage::register and takes
                // over its old disk number. A port with a hub on it is only touched when the port itself changed (the hub was
                // unplugged or replugged): its devices have their own states, which `usb_hub::rescan` follows.
                if controller.port_status(port) & 1 != 0 && !controller.port_connect_changed(port) {
                    continue;
                }
                crate::usb_serial::xhci_departed(index, port);
                usb_storage::mark_xhci_port_stale(index, port); // before the slots go: nothing may use them again (a hub's children too)
                crate::usb_hub::forget_hubs_on_port(index, port); // the hub, and any hub behind it, is gone with its port
                release_port(controller, port, None);
                crate::println!("USB: xHCI {:02x}:{:02x}.{} port {port}: its device was unplugged or replugged; looking at the port again.",
                    controller.pci.bus, controller.pci.device, controller.pci.function);
            }
            if controller.port_status(port) & 1 == 0 {
                if index < IDLE_CONTROLLERS {
                    unsafe { (*core::ptr::addr_of_mut!(IDLE_PORTS))[index] &= !(1 << port) };
                }
                continue;
            }
            if index < IDLE_CONTROLLERS && unsafe { (*core::ptr::addr_of!(IDLE_PORTS))[index] } & (1 << port) != 0 && !controller.port_connect_changed(port) {
                continue;
            }
            // Real controllers sometimes fail the first transfer after a reset; try again from
            // scratch (reset included) a couple of times before giving up, and say what failed.
            for attempt in 1..=3 {
                unsafe { STEP = "open the port (reset, enable slot, address device)" };
                match probe_port(index, controller, port) {
                    Ok(true) => {
                        added += 1;
                        break;
                    }
                    Ok(false) => break,
                    Err(error) => {
                        crate::println!(
                            "USB: xHCI {:02x}:{:02x}.{} port {port}, attempt {attempt}/3 failed at \"{}\": {error:?} (PORTSC {:#x}, USBSTS {:#x}).",
                            controller.pci.bus, controller.pci.device, controller.pci.function,
                            unsafe { STEP }, controller.port_status(port), controller.usbsts()
                        );
                        release_port(controller, port, None);
                        crate::timer::wait_ticks(15);
                    }
                }
            }
        }
    }
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        added += crate::usb_hub::rescan(index, controller);
    }
    added + probe_all_ehci() + probe_all_ohci()
}

/// `usb try PORT [hot|warm]`: a diagnostic for a port that will not enumerate: optionally resets it by hand, then goes
/// through the ordinary enumeration once and says where it stopped.
pub fn try_port(port: u8, how: Option<&str>) {
    let Some(controller) = xhci::controller(0) else { return crate::println!("usb try: no xHCI controller.") };
    crate::println!("port {port}: PORTSC {:#010x} before.", controller.port_status(port));
    match how {
        Some("hot") | Some("warm") => {
            let done = controller.reset_port_raw(port, how == Some("warm"));
            crate::println!("port {port}: {} reset {}; PORTSC {:#010x}.", how.unwrap_or(""), if done { "completed" } else { "did not complete" }, controller.port_status(port));
        }
        _ => {}
    }
    release_port(controller, port, None);
    unsafe { STEP = "open the port (reset, enable slot, address device)" };
    match probe_port(0, controller, port) {
        Ok(true) => crate::println!("port {port}: enumerated, a device was added."),
        Ok(false) => crate::println!("port {port}: enumerated, nothing to use on it."),
        Err(error) => {
            crate::println!("port {port}: failed at \"{}\": {error:?}.", unsafe { STEP });
            release_port(controller, port, None);
        }
    }
    crate::println!("port {port}: PORTSC {:#010x} after, USBSTS {:#x}.", controller.port_status(port), controller.usbsts());
}

static mut STEP: &str = "";

/// Names the enumeration step in progress, for the "attempt failed at ..." message (the hub setup in `usb_hub` uses it too).
pub fn set_step(step: &'static str) {
    unsafe { STEP = step };
}

pub fn step() -> &'static str {
    unsafe { STEP }
}

fn probe_port(controller_index: usize, controller: &mut Controller, port: u8) -> Result<bool, xhci::Error> {
    let device = controller.open_port(port)?;
    probe_device(controller_index, controller, device)
}

/// Identifies an addressed device (on a root port or behind a hub) and starts whatever it
/// is: a boot keyboard, a Mass Storage disk or a hub whose own ports are then scanned.
pub fn probe_device(controller_index: usize, controller: &mut Controller, mut device: xhci::Device) -> Result<bool, xhci::Error> {
    unsafe { STEP = "read the device descriptor (first 8 bytes)" };
    let mut descriptor = [0u8; 18];
    // The first 8 bytes of the device descriptor hold bMaxPacketSize0; a full-speed
    // device may differ from the guess made when it was addressed.
    controller.get_descriptor(&device, 1, 0, &mut descriptor[..8])?;
    unsafe { STEP = "set the control max packet size" };
    // SuperSpeed devices give bMaxPacketSize0 as an exponent (9 means 2^9 = 512 bytes; it is always 9), the other speeds as the
    // size itself. Taking the 9 as a size set the default pipe to 9 bytes and every later control transfer on a SuperSpeed
    // device timed out.
    let max_packet = if device.speed >= 4 { 512 } else { u16::from(descriptor[7]).max(8) };
    controller.set_control_max_packet(&mut device, max_packet)?;
    unsafe { STEP = "read the full device descriptor" };
    controller.get_descriptor(&device, 1, 0, &mut descriptor)?;
    unsafe { STEP = "read the configuration descriptor" };
    let (vendor, product) = (u16::from_le_bytes([descriptor[8], descriptor[9]]), u16::from_le_bytes([descriptor[10], descriptor[11]]));
    let port = device.path();
    if descriptor[4] == 9 {
        let mut configuration = [0u8; 9];
        controller.get_descriptor(&device, 2, 0, &mut configuration)?;
        crate::println!("USB: port {port}: a hub ({vendor:04x}:{product:04x}, {}).", xhci::speed_name(device.speed));
        return crate::usb_hub::enumerate(controller_index, controller, device, configuration[5]);
    }

    let mut configuration = [0u8; 2048];
    controller.get_descriptor(&device, 2, 0, &mut configuration[..9])?;
    let total = usize::from(u16::from_le_bytes([configuration[2], configuration[3]])).clamp(9, configuration.len());
    controller.get_descriptor(&device, 2, 0, &mut configuration[..total])?;
    let Some((interface, endpoint, max_packet_in, interval)) = find_boot_keyboard(&configuration[..total]) else {
        // Not a keyboard: a USB stick (Mass Storage, Bulk-Only) becomes a disk.
        if let Some(endpoints) = usb_storage::find_endpoints(&configuration[..total]) {
            controller.set_configuration(&device, configuration[5])?;
            return Ok(match usb_storage::attach_xhci(controller_index, controller, device, endpoints) {
                Some(disk) => {
                    crate::println!("USB storage {vendor:04x}:{product:04x} on port {port} of xHCI {:02x}:{:02x}.{} ({}): {}.", controller.pci.bus, controller.pci.device, controller.pci.function, xhci::speed_name(device.speed), crate::storage::DiskNumber(disk));
                    true
                }
                None => {
                    controller.release_slot(device.slot);
                    false
                }
            });
        }
        // A device can be audio, MIDI or both (a class-compliant interface such as the RME UFX has PCM streaming and a MIDI port
        // in one configuration). Audio goes first: attaching it selects the configuration, which would reset the bulk endpoints
        // the MIDI side had configured. Each side takes what it can; the slot is only released when neither did.
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
        match crate::usb_serial::attach_xhci(controller_index, controller, device, vendor, product, descriptor[17]) {
            crate::usb_serial::Outcome::Attached => {
                crate::println!("USB serial {vendor:04x}:{product:04x} on port {port} of xHCI {:02x}:{:02x}.{} ({}): ready (see `usb`).", controller.pci.bus, controller.pci.device, controller.pci.function, xhci::speed_name(device.speed));
                return Ok(true);
            }
            crate::usb_serial::Outcome::Reprobe | crate::usb_serial::Outcome::Retry => {
                controller.release_slot(device.slot); // the port is reset and the device enumerated afresh on the next pass
                return Ok(false);
            }
            crate::usb_serial::Outcome::Unclaimed => {}
        }
        if crate::usb_pointer::claim(controller, &device, &configuration[..total], vendor, product) {
            return Ok(true);
        }
        controller.release_slot(device.slot);
        crate::println!("USB: port {port}: {vendor:04x}:{product:04x} (device class {:#04x}, {}): nothing here uses it.", descriptor[4], xhci::speed_name(device.speed));
        if device.route == 0 && controller_index < IDLE_CONTROLLERS && device.port < 64 {
            unsafe { (*core::ptr::addr_of_mut!(IDLE_PORTS))[controller_index] |= 1 << device.port };
        }
        return Ok(false);
    };

    controller.set_configuration(&device, configuration[5])?;
    // HID class requests: SET_PROTOCOL(boot) and SET_IDLE(0) (report only on change).
    // Some keyboards stall SET_IDLE; that is harmless.
    controller.control_transfer(&device, 0x21, 0x0b, 0, u16::from(interface), &mut [])?;
    let _ = controller.control_transfer(&device, 0x21, 0x0a, 0, u16::from(interface), &mut []);
    let dci = controller.configure_interrupt_in(&device, endpoint, max_packet_in, interval)?;

    let buffer = memory::allocate_contiguous(1, Some(1 << 32)).ok_or(xhci::Error::NoMemory)?;
    let slot = unsafe { (*core::ptr::addr_of_mut!(KEYBOARDS)).iter_mut().find(|entry| entry.is_none()) };
    let Some(slot) = slot else {
        controller.release_slot(device.slot);
        return Ok(false);
    };
    *slot = Some(Keyboard {
        source: Source::Xhci { bus: controller.pci.bus, device: controller.pci.device, function: controller.pci.function, slot: device.slot, dci, buffer },
        previous: [0; 8],
        caps: false,
        reports: 0,
    });
    controller.queue_interrupt_in(device.slot, dci, buffer, 8);
    crate::println!(
        "USB keyboard {vendor:04x}:{product:04x} on port {port} of xHCI {:02x}:{:02x}.{} ({}): ready.",
        controller.pci.bus,
        controller.pci.device,
        controller.pci.function,
        xhci::speed_name(device.speed)
    );
    Ok(true)
}

/// Walks a configuration descriptor for a HID boot keyboard interface and its
/// interrupt-IN endpoint. Returns (interface number, endpoint address, max packet, bInterval).
fn find_boot_keyboard(descriptors: &[u8]) -> Option<(u8, u8, u16, u8)> {
    let mut at = 0;
    let mut interface = None;
    while at + 2 <= descriptors.len() {
        let length = usize::from(descriptors[at]);
        if length < 2 || at + length > descriptors.len() {
            break;
        }
        match descriptors[at + 1] {
            4 if length >= 9 => {
                // interface: number, class 3 (HID), subclass 1 (boot), protocol 1 (keyboard)
                interface = (descriptors[at + 5] == 3 && descriptors[at + 6] == 1 && descriptors[at + 7] == 1).then_some(descriptors[at + 2]);
            }
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

/// Handles an event nobody was waiting for: a keyboard's report completing.
pub fn background_event(controller: &mut Controller, event: &Trb) {
    if !event.is_transfer() {
        return;
    }
    // An unplugged device's endpoints report errors, and every handler below answers one with Reset Endpoint commands (each
    // timing out, since the controller already dropped the slot) and a re-armed transfer, which fails again: the command
    // ring was aborted over and over and the disks behind the same controller stalled. Root port empty: drop the event and
    // do not re-arm. (A device behind a hub is covered by its root port too; `usb rescan` cleans up the registrations.)
    if !matches!(event.completion(), xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT)
        && controller.slot_location(event.slot_number()).is_some_and(|(port, _)| port != 0 && controller.port_status(port) & 1 == 0)
    {
        return;
    }
    if crate::usb_midi::background_event(controller, event) || crate::usb_pointer::background_event(controller, event) || crate::usb_serial::background_event(controller, event) {
        return;
    }
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    let Some(keyboard) = keyboards.iter_mut().flatten().find(|k| {
        matches!(k.source, Source::Xhci { bus, device, function, slot, dci, .. }
            if slot == event.slot_number() && dci == event.endpoint_index() && (bus, device, function) == identity)
    }) else {
        return;
    };
    let Source::Xhci { slot, dci, buffer, .. } = keyboard.source else { return };
    match event.completion() {
        xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT => {
            let report: [u8; 8] = unsafe { core::ptr::read_volatile(buffer as *const [u8; 8]) };
            keyboard.reports += 1;
            decode(keyboard, &report);
        }
        _ => controller.reset_stalled_endpoint(slot, dci), // un-halt, then re-arm below
    }
    controller.queue_interrupt_in(slot, dci, buffer, 8);
}

/// Keys present now that were not in the previous report are new presses.
fn decode(keyboard: &mut Keyboard, report: &[u8; 8]) {
    let modifiers = report[0];
    for &usage in &report[2..8] {
        if usage < 4 || keyboard.previous[2..8].contains(&usage) {
            continue; // none, rollover error, or still held
        }
        if usage == 0x39 {
            keyboard.caps = !keyboard.caps;
        } else if let Some(byte) = hid_key(usage, modifiers, keyboard.caps, keyboard::layout()) {
            keyboard::push(byte);
        }
    }
    keyboard.previous = *report;
}

/// Drains every controller's event ring and polls every EHCI keyboard queue (call
/// from the main loop).
pub fn poll() {
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        while let Some(event) = controller.next_event() {
            background_event(controller, &event);
        }
    }
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    for keyboard in keyboards.iter_mut().flatten() {
        if let Source::Ehci { controller, index } = keyboard.source {
            if let Some(report) = ehci::controller(controller).and_then(|c| c.poll_keyboard(index)) {
                keyboard.reports += 1;
                decode(keyboard, &report);
            }
        }
        if let Source::Ohci { controller, index } = keyboard.source {
            if let Some(report) = ohci::controller(controller).and_then(|c| c.poll_keyboard(index)) {
                keyboard.reports += 1;
                decode(keyboard, &report);
            }
        }
    }
}

/// Probes every EHCI controller's connected ports (not yet claimed by a keyboard)
/// for boot keyboards, directly or behind a hub. Returns how many were added.
fn probe_all_ehci() -> usize {
    let mut added = 0;
    for index in 0..ehci::count() {
        let Some(controller) = ehci::controller(index) else { continue };
        for port in 1..=controller.ports {
            if controller.claimed_ports & (1 << port) != 0 {
                if !controller.port_connect_changed(port) {
                    // Still the device we enumerated. If it is a hub, its own ports may have changed: nothing tells the
                    // host, so ask the hub.
                    added += rescan_ehci_hub(index, controller, port);
                    continue;
                }
                // The root port itself changed: a hub on it is gone with everything behind it.
                if let Some(hub_address) = controller.hub_address_on_port(port) {
                    usb_storage::mark_ehci_hub_stale(index, hub_address, 0);
                    crate::usb_audio::ehci_departed(index, hub_address, 0);
                    crate::usb_serial::ehci_departed(index, hub_address, 0);
                    controller.forget_hub(port);
                } else {
                    crate::usb_audio::ehci_departed(index, 0, 0);
                    crate::usb_serial::ehci_root_departed(index, port);
                }
                // It was unplugged, or unplugged and plugged back in (a bumped cable, a stick moved to another machine and
                // returned). Forget the claim and look at the port afresh; a disk that comes back is recognised in
                // usb_storage::register and takes over its old disk number.
                controller.claimed_ports &= !(1 << port);
                forget_ehci_keyboards(index, controller, port);
                crate::println!("USB: EHCI {:02x}:{:02x}.{} port {port}: its device was unplugged or replugged; looking at the port again.",
                    controller.pci.bus, controller.pci.device, controller.pci.function);
            }
            if !controller.port_has_device(port) {
                continue;
            }
            let mut found = [None; 8];
            let mut attempt = 1;
            let count = loop {
                match controller.enumerate_port(port, &mut found) {
                    // NoDevice mid-way is a bounce, not necessarily a removal: wait for the connection again (below) and retry.
                    Err(error @ (ehci::Error::Timeout | ehci::Error::Transfer(_) | ehci::Error::NoDevice)) if attempt < 5 => {
                        crate::println!(
                            "USB: EHCI {:02x}:{:02x}.{} port {port}, attempt {attempt}/5 failed at \"{}\": {error:?} (PORTSC {:#x}, USBSTS {:#x}).",
                            controller.pci.bus, controller.pci.device, controller.pci.function, controller.step, controller.port_status(port), controller.usbsts()
                        );
                        attempt += 1;
                        crate::timer::wait_ticks(15);
                        if !controller.wait_port_stable(port, 100, 1500) {
                            break Err(ehci::Error::NoDevice); // it really is gone
                        }
                    }
                    other => break other,
                }
            };
            let count = match count {
                Ok(count) => count,
                Err(ehci::Error::CompanionPort) => {
                    crate::println!("USB: EHCI {:02x}:{:02x}.{} port {port}: a full/low-speed device wired to the root port belongs to the companion controller (OHCI, see `usb`; UHCI has no driver).", controller.pci.bus, controller.pci.device, controller.pci.function);
                    continue;
                }
                Err(error) => {
                    crate::println!(
                        "USB: EHCI {:02x}:{:02x}.{} port {port}: device not usable after {attempt} attempt(s): {error:?} at \"{}\" (PORTSC {:#x}, USBSTS {:#x}).",
                        controller.pci.bus, controller.pci.device, controller.pci.function, controller.step, controller.port_status(port), controller.usbsts()
                    );
                    continue;
                }
            };
            for device in found[..count].iter().flatten() {
                match setup_ehci_keyboard(index, controller, device, if device.target.hub_address == 0 { port } else { 0 }, port) {
                    Ok(true) => {
                        controller.claimed_ports |= 1 << port;
                        added += 1;
                    }
                    Ok(false) => {}
                    Err(error) => crate::println!("USB: EHCI port {port}: device not usable ({error:?})."),
                }
            }
        }
    }
    added
}

/// Forgets the keyboards of an EHCI root port whose device went away: their queue heads leave the async list and their
/// table entries are freed (a stale entry kept being polled, and the table filled up after four replugs).
fn forget_ehci_keyboards(controller_index: usize, controller: &mut ehci::Controller, port: u8) {
    let removed = controller.remove_keyboards_on_port(port);
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    for entry in keyboards.iter_mut() {
        if matches!(*entry, Some(Keyboard { source: Source::Ehci { controller: c, index }, .. }) if c == controller_index && removed & (1 << index) != 0) {
            *entry = None;
        }
    }
}

/// Asks the hub on EHCI root `port` about its ports: a disk or keyboard that went away is forgotten (disk handles go stale,
/// so a returning stick takes its old number back in `usb_storage::register`), and what appeared is set up. Returns how
/// many devices were added.
fn rescan_ehci_hub(controller_index: usize, controller: &mut ehci::Controller, port: u8) -> usize {
    let Some(hub_address) = controller.hub_address_on_port(port) else { return 0 };
    let mut found = [None; 8];
    let (departed, count) = controller.rescan_hub(port, &mut found);
    for hub_port in 1..16u8 {
        if departed & (1 << hub_port) == 0 {
            continue;
        }
        crate::println!("USB: EHCI {:02x}:{:02x}.{} hub port {port}.{hub_port}: its device was unplugged or replugged; looking at the port again.",
            controller.pci.bus, controller.pci.device, controller.pci.function);
        usb_storage::mark_ehci_hub_stale(controller_index, hub_address, hub_port);
        crate::usb_audio::ehci_departed(controller_index, hub_address, hub_port);
        crate::usb_serial::ehci_departed(controller_index, hub_address, hub_port);
        let removed = controller.remove_keyboards_behind(hub_address, hub_port);
        drop_ehci_keyboard_entries(controller_index, removed);
    }
    let mut added = 0;
    for device in found[..count].iter().flatten() {
        match setup_ehci_keyboard(controller_index, controller, device, 0, port) {
            Ok(true) => added += 1,
            Ok(false) => {}
            Err(error) => crate::println!("USB: EHCI port {port}: device not usable ({error:?})."),
        }
    }
    added
}

fn drop_ehci_keyboard_entries(controller_index: usize, removed: u8) {
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    for entry in keyboards.iter_mut() {
        if matches!(*entry, Some(Keyboard { source: Source::Ehci { controller: c, index }, .. }) if c == controller_index && removed & (1 << index) != 0) {
            *entry = None;
        }
    }
}

/// Probes every OHCI controller's root ports for boot keyboards (OHCI is the companion that gets the low- and full-speed devices
/// when a board's EHCI controller takes the high-speed ones; there are no hubs or disks here). Returns how many were added. A
/// port whose device was unplugged or replugged since it was claimed has its keyboard dropped and is looked at again.
fn probe_all_ohci() -> usize {
    let mut added = 0;
    for index in 0..ohci::count() {
        let Some(controller) = ohci::controller(index) else { continue };
        for port in 1..=controller.ports {
            if controller.claimed_ports & (1 << port) != 0 {
                if !controller.port_connect_changed(port) {
                    continue; // still the device we enumerated
                }
                controller.claimed_ports &= !(1 << port);
                let removed = controller.remove_keyboards_on_port(port);
                drop_ohci_keyboard_entries(index, removed);
                crate::println!("USB: OHCI {:02x}:{:02x}.{} port {port}: its device was unplugged or replugged; looking at the port again.",
                    controller.pci.bus, controller.pci.device, controller.pci.function);
            }
            if !controller.port_has_device(port) {
                continue;
            }
            let mut attempt = 1;
            let found = loop {
                match controller.enumerate_port(port) {
                    Err(error) if attempt < 3 => {
                        crate::println!("USB: OHCI {:02x}:{:02x}.{} port {port}, attempt {attempt}/3 failed at \"{}\": {error:?}.",
                            controller.pci.bus, controller.pci.device, controller.pci.function, controller.step);
                        attempt += 1;
                        crate::timer::wait_ticks(15);
                    }
                    other => break other,
                }
            };
            let found = match found {
                Ok(found) => found,
                Err(error) => {
                    crate::println!("USB: OHCI {:02x}:{:02x}.{} port {port}: device not usable ({error:?} at \"{}\").",
                        controller.pci.bus, controller.pci.device, controller.pci.function, controller.step);
                    continue;
                }
            };
            if found.descriptor[4] == 9 {
                crate::println!("USB: OHCI {:02x}:{:02x}.{} port {port}: a hub; hubs on OHCI are not supported.", controller.pci.bus, controller.pci.device, controller.pci.function);
                continue;
            }
            match setup_ohci_keyboard(index, controller, &found, port) {
                Ok(true) => {
                    controller.claimed_ports |= 1 << port;
                    added += 1;
                }
                Ok(false) => {}
                Err(error) => crate::println!("USB: OHCI port {port}: device not usable ({error:?})."),
            }
        }
    }
    added
}

fn drop_ohci_keyboard_entries(controller_index: usize, removed: u8) {
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    for entry in keyboards.iter_mut() {
        if matches!(*entry, Some(Keyboard { source: Source::Ohci { controller: c, index }, .. }) if c == controller_index && removed & (1 << index) != 0) {
            *entry = None;
        }
    }
}

fn setup_ohci_keyboard(controller_index: usize, controller: &mut ohci::Controller, found: &ohci::Found, root_port: u8) -> Result<bool, ohci::Error> {
    let target = found.target;
    let (vendor, product) = (u16::from_le_bytes([found.descriptor[8], found.descriptor[9]]), u16::from_le_bytes([found.descriptor[10], found.descriptor[11]]));
    let mut configuration = [0u8; 256];
    controller.get_descriptor(&target, 2, 0, &mut configuration[..9])?;
    let total = usize::from(u16::from_le_bytes([configuration[2], configuration[3]])).clamp(9, configuration.len());
    controller.get_descriptor(&target, 2, 0, &mut configuration[..total])?;
    let Some((interface, endpoint, max_packet, _interval)) = find_boot_keyboard(&configuration[..total]) else {
        return Ok(false);
    };
    controller.set_configuration(&target, configuration[5])?;
    controller.control_transfer(&target, 0x21, 0x0b, 0, u16::from(interface), &mut [])?; // SET_PROTOCOL(boot)
    let _ = controller.control_transfer(&target, 0x21, 0x0a, 0, u16::from(interface), &mut []); // SET_IDLE(0)
    let index = controller.add_keyboard(target, endpoint, max_packet, root_port)?;
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    let Some(slot) = keyboards.iter_mut().find(|entry| entry.is_none()) else { return Ok(false) };
    *slot = Some(Keyboard { source: Source::Ohci { controller: controller_index, index }, previous: [0; 8], caps: false, reports: 0 });
    crate::println!(
        "USB keyboard {vendor:04x}:{product:04x} on OHCI {:02x}:{:02x}.{} port {root_port} ({}): ready.",
        controller.pci.bus,
        controller.pci.device,
        controller.pci.function,
        if target.low_speed { "low speed" } else { "full speed" }
    );
    Ok(true)
}

fn setup_ehci_keyboard(controller_index: usize, controller: &mut ehci::Controller, found: &ehci::Found, root_port: u8, claimed_port: u8) -> Result<bool, ehci::Error> {
    let target = found.target;
    let (vendor, product) = (u16::from_le_bytes([found.descriptor[8], found.descriptor[9]]), u16::from_le_bytes([found.descriptor[10], found.descriptor[11]]));
    let mut configuration = [0u8; 256];
    controller.get_descriptor(&target, 2, 0, &mut configuration[..9])?;
    let total = usize::from(u16::from_le_bytes([configuration[2], configuration[3]])).clamp(9, configuration.len());
    controller.get_descriptor(&target, 2, 0, &mut configuration[..total])?;
    let Some((interface, endpoint, max_packet, _interval)) = find_boot_keyboard(&configuration[..total]) else {
        if let Some(endpoints) = usb_storage::find_endpoints(&configuration[..total]) {
            controller.set_configuration(&target, configuration[5])?;
            if let Some(disk) = usb_storage::attach_ehci(controller_index, target, root_port, endpoints) {
                crate::println!("USB storage {vendor:04x}:{product:04x} on EHCI {:02x}:{:02x}.{}: {}.", controller.pci.bus, controller.pci.device, controller.pci.function, crate::storage::DiskNumber(disk));
                return Ok(true);
            }
        }
        if crate::usb_audio::is_audio(&configuration[..total]) {
            let description = crate::usb_audio::parse(&configuration[..total]);
            let speed = match target.speed {
                ehci::Speed::High => "high speed",
                ehci::Speed::Full => "full speed",
                ehci::Speed::Low => "low speed",
            };
            crate::usb_audio::report(&description, &format_args!("EHCI {:02x}:{:02x}.{} address {}", controller.pci.bus, controller.pci.device, controller.pci.function, target.address), vendor, product, speed);
            if crate::usb_audio::attach_ehci(controller, controller_index, target, description, configuration[5]) {
                return Ok(true);
            }
        }
        match crate::usb_serial::attach_ehci(controller_index, controller, target, root_port, vendor, product, found.descriptor[17]) {
            crate::usb_serial::Outcome::Attached => {
                crate::println!("USB serial {vendor:04x}:{product:04x} on EHCI {:02x}:{:02x}.{} address {}: ready (see `usb`).", controller.pci.bus, controller.pci.device, controller.pci.function, target.address);
                return Ok(true);
            }
            crate::usb_serial::Outcome::Reprobe | crate::usb_serial::Outcome::Retry => {
                // A firmware download changes what the device is: reset its port so that it is enumerated afresh.
                if target.hub_address != 0 {
                    controller.unclaim_hub_child(target.hub_address, target.hub_port);
                }
                return Ok(false);
            }
            crate::usb_serial::Outcome::Unclaimed => {}
        }
        crate::println!(
            "USB: EHCI {:02x}:{:02x}.{} address {}: {vendor:04x}:{product:04x} (device class {:#04x}): nothing here uses it.",
            controller.pci.bus,
            controller.pci.device,
            controller.pci.function,
            target.address,
            found.descriptor[4]
        );
        return Ok(false);
    };
    controller.set_configuration(&target, configuration[5])?;
    controller.control_transfer(&target, 0x21, 0x0b, 0, u16::from(interface), &mut [])?; // SET_PROTOCOL(boot)
    let _ = controller.control_transfer(&target, 0x21, 0x0a, 0, u16::from(interface), &mut []); // SET_IDLE(0)
    let index = controller.add_keyboard(target, endpoint, max_packet, claimed_port)?;
    let keyboards = unsafe { &mut *core::ptr::addr_of_mut!(KEYBOARDS) };
    let Some(slot) = keyboards.iter_mut().find(|entry| entry.is_none()) else { return Ok(false) };
    *slot = Some(Keyboard { source: Source::Ehci { controller: controller_index, index }, previous: [0; 8], caps: false, reports: 0 });
    crate::println!(
        "USB keyboard {vendor:04x}:{product:04x} on EHCI {:02x}:{:02x}.{} ({}{}): ready.",
        controller.pci.bus,
        controller.pci.device,
        controller.pci.function,
        match target.speed {
            ehci::Speed::High => "high speed",
            ehci::Speed::Full => "full speed, split transactions",
            ehci::Speed::Low => "low speed, split transactions",
        },
        if target.hub_address != 0 { ", behind a hub" } else { "" }
    );
    Ok(true)
}

pub fn describe_devices(print: impl Fn(core::fmt::Arguments)) {
    crate::usbshell::describe_xhci(&print);
    describe_other_controllers(&print);
    print(format_args!("{} USB keyboard(s), {} reports so far", keyboard_count(), reports_seen()));
}

/// EHCI and OHCI controllers and the MIDI devices (the xHCI part is shared with the Pi: `usbshell.rs`).
pub fn describe_other_controllers(print: impl Fn(core::fmt::Arguments)) {
    for index in 0..ehci::count() {
        let Some(controller) = ehci::controller(index) else { continue };
        print(format_args!("EHCI {:02x}:{:02x}.{}: {} ports", controller.pci.bus, controller.pci.device, controller.pci.function, controller.ports));
        controller.describe_keyboards(&print);
        for port in 1..=controller.ports {
            if controller.port_has_device(port) {
                print(format_args!("  port {port}: device connected{}", if controller.claimed_ports & (1 << port) != 0 { ", in use (a keyboard or a disk)" } else { "" }));
            }
        }
    }
    for index in 0..ohci::count() {
        let Some(controller) = ohci::controller(index) else { continue };
        print(format_args!("OHCI {:02x}:{:02x}.{}: {} ports", controller.pci.bus, controller.pci.device, controller.pci.function, controller.ports));
        for port in 1..=controller.ports {
            if let Some((low_speed, claimed)) = controller.describe_port(port) {
                print(format_args!("  port {port}: device connected ({}){}", if low_speed { "low speed" } else { "full speed" }, if claimed { ", in use (a keyboard)" } else { "" }));
            }
        }
    }
    crate::usb_midi::describe_devices(&print);
}
