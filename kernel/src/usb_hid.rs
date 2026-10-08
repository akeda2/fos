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

use fos_v2_abi::keymap::hid_ascii;

use crate::xhci::{self, Controller, Trb};
use crate::{ehci, usb_storage};
use crate::{keyboard, memory};

const MAX_KEYBOARDS: usize = 4;

/// Where a keyboard's reports come from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    /// An xHCI slot's interrupt endpoint; the controller is identified by its PCI address.
    Xhci { bus: u8, device: u8, function: u8, slot: u8, dci: u32, buffer: u64 },
    /// A permanent queue head on an EHCI controller.
    Ehci { controller: usize, index: usize },
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
    let mut added = 0;
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        for port in 1..=controller.ports {
            if controller.slot_in_use_for_port(port) {
                // A device we enumerated here earlier. If the port says it went away (or went away and came back), drop its
                // slot and look at the port afresh; a disk that returns is recognised in usb_storage::register and takes
                // over its old disk number. A port with a hub on it is left alone (its devices have their own states).
                if crate::usb_hub::has_hub_on_port(index, port) || (controller.port_status(port) & 1 != 0 && !controller.port_connect_changed(port)) {
                    continue;
                }
                usb_storage::mark_xhci_port_stale(index, port); // before the slots go: nothing may use them again
                release_port(controller, port, None);
                crate::println!("USB: xHCI {:02x}:{:02x}.{} port {port}: its device was unplugged or replugged; looking at the port again.",
                    controller.pci.bus, controller.pci.device, controller.pci.function);
            }
            if controller.port_status(port) & 1 == 0 {
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
                        crate::apic::wait_ticks(15);
                    }
                }
            }
        }
    }
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        added += crate::usb_hub::rescan(index, controller);
    }
    added + probe_all_ehci()
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
        // Not a keyboard, not a stick: an audio device is described (see `usb_audio`); nothing else is handled.
        if crate::usb_audio::is_audio(&configuration[..total]) {
            let description = crate::usb_audio::parse(&configuration[..total]);
            crate::usb_audio::report(&description, &device.path(), vendor, product, xhci::speed_name(device.speed));
            if crate::usb_audio::attach(controller, device, description, configuration[5]) {
                return Ok(true);
            }
            controller.release_slot(device.slot);
            return Ok(false);
        }
        controller.release_slot(device.slot);
        crate::println!("USB: port {port}: {vendor:04x}:{product:04x} (device class {:#04x}, {}): nothing here uses it.", descriptor[4], xhci::speed_name(device.speed));
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
        } else if let Some(byte) = hid_ascii(usage, modifiers, keyboard.caps) {
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
                    controller.forget_hub(port);
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
                        crate::apic::wait_ticks(15);
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
                    crate::println!("USB: EHCI {:02x}:{:02x}.{} port {port}: a full/low-speed device wired to the root port belongs to the companion controller, which has no driver.", controller.pci.bus, controller.pci.device, controller.pci.function);
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
    for index in 0..xhci::count() {
        let Some(controller) = xhci::controller(index) else { continue };
        print(format_args!("xHCI {:02x}:{:02x}.{}: {} ports, events by {}", controller.pci.bus, controller.pci.device, controller.pci.function, controller.ports, if !controller.interrupt_driven { "polling" } else if controller.uses_msix { "MSI-X" } else { "MSI" }));
        for port in 1..=controller.ports {
            let status = controller.port_status(port);
            if status & 1 != 0 {
                let speed = ((status >> 10) & 0xf) as u8;
                print(format_args!("  port {port}: device connected ({}){}", xhci::speed_name(speed), if controller.slot_in_use_for_port(port) { ", in use" } else { "" }));
            }
        }
        crate::usb_hub::describe(index, controller, &print);
    }
    for index in 0..ehci::count() {
        let Some(controller) = ehci::controller(index) else { continue };
        print(format_args!("EHCI {:02x}:{:02x}.{}: {} ports", controller.pci.bus, controller.pci.device, controller.pci.function, controller.ports));
        for port in 1..=controller.ports {
            if controller.port_has_device(port) {
                print(format_args!("  port {port}: device connected{}", if controller.claimed_ports & (1 << port) != 0 { ", in use (a keyboard or a disk)" } else { "" }));
            }
        }
    }
    print(format_args!("{} USB keyboard(s), {} reports so far", keyboard_count(), reports_seen()));
}
