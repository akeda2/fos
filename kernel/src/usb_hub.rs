//! USB hubs behind an xHCI controller: find the devices on a hub's ports and hand each
//! one to the ordinary device probe (keyboard, Mass Storage, another hub).
//!
//! What a hub needs from the host. A hub is an ordinary USB device of class 9 whose
//! *ports* are controlled with class requests on its default pipe: `SET_FEATURE
//! PORT_POWER` switches a port on, `GET_STATUS` returns a 32-bit word (low half: the
//! port's state, high half: which bits changed), `SET_FEATURE PORT_RESET` resets the
//! device on it, and `CLEAR_FEATURE C_*` acknowledges a change. This driver polls those
//! (no status-change interrupt endpoint), which is enough to find devices at boot and on
//! `usb rescan`; unplugging a hub's device is not noticed until then.
//!
//! What the *controller* needs. The xHCI must be told a slot is a hub (the Hub flag and
//! port count in its slot context, `Controller::configure_hub`), and every device behind
//! it is addressed with a *route string* (which hub port to take at each tier) next to the
//! root port number. A low- or full-speed device behind a high-speed hub also needs the
//! hub's *transaction translator*: the controller then sends it split transactions
//! through that hub (`Device::tt`, filled in by `Controller::open_hub_port`).
//!
//! Limits: USB 2 and USB 3 hubs, up to `MAX_HUBS` of them, four tiers deep; one transaction
//! translator per hub is assumed (multi-TT hubs work, just without the extra parallelism).

use crate::xhci::{self, Controller, Device};

pub const MAX_HUBS: usize = 8;

const REQUEST_GET_STATUS: u8 = 0;
const REQUEST_CLEAR_FEATURE: u8 = 1;
const REQUEST_SET_FEATURE: u8 = 3;
const REQUEST_GET_DESCRIPTOR: u8 = 6;
const REQUEST_SET_HUB_DEPTH: u8 = 12;
const FEATURE_PORT_RESET: u16 = 4;
const FEATURE_PORT_POWER: u16 = 8;
const FEATURE_C_PORT_CONNECTION: u16 = 16;
const FEATURE_C_PORT_RESET: u16 = 20;

const STATUS_CONNECTED: u32 = 1 << 0;
const STATUS_ENABLED: u32 = 1 << 1;
const STATUS_LOW_SPEED: u32 = 1 << 9;
const STATUS_HIGH_SPEED: u32 = 1 << 10;
const CHANGE_CONNECTION: u32 = 1 << 16;
const CHANGE_RESET: u32 = 1 << (16 + 4);

#[derive(Clone, Copy)]
struct Hub {
    controller: usize,
    device: Device,
    ports: u8,
}

static mut HUBS: [Option<Hub>; MAX_HUBS] = [None; MAX_HUBS];

fn hubs() -> &'static mut [Option<Hub>; MAX_HUBS] {
    unsafe { &mut *core::ptr::addr_of_mut!(HUBS) }
}

/// Reads a port's status and change word.
fn port_status(controller: &mut Controller, hub: &Device, port: u8) -> Result<u32, xhci::Error> {
    let mut data = [0u8; 4];
    controller.control_transfer(hub, 0xa3, REQUEST_GET_STATUS, 0, u16::from(port), &mut data)?;
    Ok(u32::from_le_bytes(data))
}

fn port_feature(controller: &mut Controller, hub: &Device, request: u8, feature: u16, port: u8) -> Result<(), xhci::Error> {
    controller.control_transfer(hub, 0x23, request, feature, u16::from(port), &mut []).map(|_| ())
}

/// Starts a hub that was just addressed: configures its slot, reads how many ports it has,
/// powers them and scans for devices. `configuration` is its only configuration's value.
pub fn enumerate(controller_index: usize, controller: &mut Controller, device: Device, configuration: u8) -> Result<bool, xhci::Error> {
    let super_speed = device.speed >= 4;
    controller.set_configuration(&device, configuration)?;
    if super_speed {
        // A SuperSpeed hub must be told how many hubs lie between it and the root.
        controller.control_transfer(&device, 0x20, REQUEST_SET_HUB_DEPTH, u16::from(device.tier), 0, &mut [])?;
    }
    let mut descriptor = [0u8; 12];
    let kind: u16 = if super_speed { 0x2a } else { 0x29 }; // hub descriptor types for USB 3 / USB 2
    controller.control_transfer(&device, 0xa0, REQUEST_GET_DESCRIPTOR, kind << 8, 0, &mut descriptor)?;
    let ports = descriptor[2];
    let characteristics = u16::from_le_bytes([descriptor[3], descriptor[4]]);
    let power_good_ms = u64::from(descriptor[5]) * 2; // bPwrOn2PwrGood is in 2 ms units
    if ports == 0 || ports > 15 {
        crate::println!("USB: hub on port {}: it reports {ports} ports; not usable.", device.path());
        controller.release_slot(device.slot);
        return Ok(false);
    }
    controller.configure_hub(&device, ports, ((characteristics >> 5) & 3) as u8)?;
    let slot = hubs().iter_mut().find(|entry| entry.is_none());
    let Some(slot) = slot else {
        crate::println!("USB: hub on port {}: too many hubs ({} supported).", device.path(), MAX_HUBS);
        controller.release_slot(device.slot);
        return Ok(false);
    };
    let hub = Hub { controller: controller_index, device, ports };
    *slot = Some(hub);
    crate::println!("USB: hub on port {}: {ports} ports, powering them.", device.path());
    for port in 1..=ports {
        let _ = port_feature(controller, &device, REQUEST_SET_FEATURE, FEATURE_PORT_POWER, port);
    }
    // Power-good time, then time for a device to announce itself and debounce.
    crate::apic::wait_ticks(power_good_ms / 10 + 25);
    scan(controller_index, controller, &hub);
    Ok(true)
}

/// Whether the device at `route` below root `port` has vanished from the hub it hangs on: asks that hub (a cheap control
/// transfer on a device that is still there) whether anything is connected to the port. A disk behind a hub gets no
/// controller-side removal signal, and a bulk transfer to the missing device takes minutes to give up, so the storage
/// driver asks before it starts. Only "nothing connected" (or a hub that no longer answers) counts: the change bit alone
/// could be a glitch, and `scan` deals with a replug.
pub fn child_vanished(controller_index: usize, port: u8, route: u32) -> bool {
    let Some(controller) = xhci::controller(controller_index) else { return true };
    for hub in hubs().iter().flatten() {
        if hub.controller != controller_index || hub.device.port != port {
            continue;
        }
        for hub_port in 1..=hub.ports {
            if route_of(&hub.device, hub_port) == route {
                return port_status(controller, &hub.device, hub_port).map_or(true, |status| status & STATUS_CONNECTED == 0);
            }
        }
    }
    false
}

/// Looks for devices on every known hub of this controller that are not yet in use.
/// Whether a hub this driver tracks hangs off root port `port` of controller `controller_index`: the devices below it share
/// that root port's connection state, so unplug detection for the port as a whole must leave them alone.
pub fn has_hub_on_port(controller_index: usize, port: u8) -> bool {
    hubs().iter().flatten().any(|hub| hub.controller == controller_index && hub.device.port == port)
}

pub fn rescan(controller_index: usize, controller: &mut Controller) -> usize {
    let mut added = 0;
    for index in 0..MAX_HUBS {
        let Some(hub) = hubs()[index] else { continue };
        if hub.controller == controller_index {
            added += scan(controller_index, controller, &hub);
        }
    }
    added
}

fn route_of(hub: &Device, port: u8) -> u32 {
    hub.route | u32::from(port) << (4 * u32::from(hub.tier))
}

fn scan(controller_index: usize, controller: &mut Controller, hub: &Hub) -> usize {
    let mut added = 0;
    for port in 1..=hub.ports {
        let Ok(status) = port_status(controller, &hub.device, port) else { continue };
        if status & STATUS_CONNECTED == 0 {
            continue;
        }
        let route = route_of(&hub.device, port);
        if controller.slot_in_use_for_route(hub.device.port, route) {
            // A device we enumerated here earlier. The hub sets the port's connection-change bit when it goes away or is
            // replaced (we cleared it after enumerating), so a set bit means "not the device we know": mark its disk
            // stale (a returning stick takes its old number back in usb_storage::register), release its slot and any
            // keyboard in it, and enumerate the port afresh.
            if status & CHANGE_CONNECTION == 0 {
                continue;
            }
            crate::println!("USB: xHCI hub port {}.{port}: its device was unplugged or replugged; looking at the port again.", hub.device.path());
            crate::usb_storage::mark_xhci_route_stale(controller_index, hub.device.port, route);
            crate::usb_hid::release_port(controller, hub.device.port, Some(route));
            let _ = port_feature(controller, &hub.device, REQUEST_CLEAR_FEATURE, FEATURE_C_PORT_CONNECTION, port);
        }
        // Real hubs and devices sometimes fail the first attempt: start over (reset
        // included) a few times, and say what failed.
        for attempt in 1..=3 {
            match probe_port(controller_index, controller, hub, port) {
                Ok(true) => {
                    added += 1;
                    break;
                }
                Ok(false) => break,
                Err(error) => {
                    crate::println!("USB: hub port {}.{port}, attempt {attempt}/3 failed: {error:?}.", hub.device.path());
                    crate::usb_hid::release_port(controller, hub.device.port, Some(route));
                    crate::apic::wait_ticks(15);
                }
            }
        }
    }
    added
}

fn probe_port(controller_index: usize, controller: &mut Controller, hub: &Hub, port: u8) -> Result<bool, xhci::Error> {
    // Reset the port and wait for the hub to say the reset finished.
    port_feature(controller, &hub.device, REQUEST_SET_FEATURE, FEATURE_PORT_RESET, port)?;
    let start = crate::apic::ticks();
    let mut status = 0;
    while crate::apic::ticks().wrapping_sub(start) < 50 {
        status = port_status(controller, &hub.device, port)?;
        if status & CHANGE_RESET != 0 {
            break;
        }
        crate::apic::wait_ticks(1);
    }
    let _ = port_feature(controller, &hub.device, REQUEST_CLEAR_FEATURE, FEATURE_C_PORT_RESET, port);
    let _ = port_feature(controller, &hub.device, REQUEST_CLEAR_FEATURE, FEATURE_C_PORT_CONNECTION, port);
    if status & CHANGE_RESET == 0 || status & STATUS_ENABLED == 0 || status & STATUS_CONNECTED == 0 {
        return Err(xhci::Error::NoDevice);
    }
    // A USB 3 hub's ports are SuperSpeed; a USB 2 hub reports low or high speed in the
    // status word, and full speed when neither bit is set.
    let speed = if hub.device.speed >= 4 {
        hub.device.speed
    } else if status & STATUS_LOW_SPEED != 0 {
        2
    } else if status & STATUS_HIGH_SPEED != 0 {
        3
    } else {
        1
    };
    crate::apic::wait_ticks(3); // reset recovery
    let device = controller.open_hub_port(&hub.device, port, speed)?;
    crate::usb_hid::probe_device(controller_index, controller, device)
}

/// Lists hubs of this controller and what is connected to each of their ports.
pub fn describe(controller_index: usize, controller: &mut Controller, print: &impl Fn(core::fmt::Arguments)) {
    for index in 0..MAX_HUBS {
        let Some(hub) = hubs()[index] else { continue };
        if hub.controller != controller_index {
            continue;
        }
        print(format_args!("  hub on port {}: {} ports", hub.device.path(), hub.ports));
        for port in 1..=hub.ports {
            let Ok(status) = port_status(controller, &hub.device, port) else { continue };
            if status & STATUS_CONNECTED != 0 {
                let in_use = controller.slot_in_use_for_route(hub.device.port, route_of(&hub.device, port));
                print(format_args!("    port {}.{port}: device connected{}", hub.device.path(), if in_use { ", in use" } else { "" }));
            }
        }
    }
}
