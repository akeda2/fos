//! The USB class drivers' registry: what the host-controller side (`usb_hid`, `usb_kbd`) tells every class driver when a
//! device goes away or an event nobody waited for arrives.
//!
//! Before this table the two enumeration files named each class driver (audio, MIDI, pointer, serial) at every unplug and
//! event site, so a driver could not be moved out of the kernel without them. Now they call the `*_all` functions here and
//! a class driver is one `Class` entry. Order is the order the calls were made in before: MIDI, pointer, then serial for
//! events and releases. Attaching a new device is still done by each enumerator (the descriptors differ per class);
//! `docs/big-kernel.md`, stage 1a.

use crate::xhci::{Controller, Trb};

/// The hooks a class driver may implement; a driver leaves out what does not concern it.
pub struct Class {
    /// The device on root port `port` (behind hub route `route`, or everything on the port when `None`) is being released.
    pub release: Option<fn(&mut Controller, u8, Option<u32>)>,
    /// An xHCI event with no waiter; true when this driver consumed it.
    pub background_event: Option<fn(&mut Controller, &Trb) -> bool>,
    /// The device on xHCI root port `port` went away.
    pub xhci_departed: Option<fn(usize, u8)>,
    /// The device behind EHCI hub `hub_address` (hub port 0: everything behind it) went away.
    pub ehci_departed: Option<fn(usize, u8, u8)>,
    /// The device directly on EHCI root port `port` went away.
    pub ehci_root_departed: Option<fn(usize, u8)>,
}

const NONE: Class = Class { release: None, background_event: None, xhci_departed: None, ehci_departed: None, ehci_root_departed: None };

static CLASSES: [Class; 4] = [
    Class { ehci_departed: Some(crate::usb_audio::ehci_departed), ..NONE },
    Class { release: Some(crate::usb_midi::release), background_event: Some(crate::usb_midi::background_event), ..NONE },
    Class { release: Some(crate::usb_pointer::release), background_event: Some(crate::usb_pointer::background_event), ..NONE },
    Class {
        background_event: Some(crate::usb_serial::background_event),
        xhci_departed: Some(crate::usb_serial::xhci_departed),
        ehci_departed: Some(crate::usb_serial::ehci_departed),
        ehci_root_departed: Some(crate::usb_serial::ehci_root_departed),
        ..NONE
    },
];

pub fn release_all(controller: &mut Controller, port: u8, route: Option<u32>) {
    for hook in CLASSES.iter().filter_map(|class| class.release) {
        hook(controller, port, route);
    }
}

/// True when a class driver took the event.
pub fn background_event(controller: &mut Controller, event: &Trb) -> bool {
    CLASSES.iter().filter_map(|class| class.background_event).any(|hook| hook(controller, event))
}

pub fn xhci_departed_all(controller_index: usize, port: u8) {
    for hook in CLASSES.iter().filter_map(|class| class.xhci_departed) {
        hook(controller_index, port);
    }
}

#[allow(dead_code)] // EHCI enumeration is x86-64 only
pub fn ehci_departed_all(controller_index: usize, hub_address: u8, hub_port: u8) {
    for hook in CLASSES.iter().filter_map(|class| class.ehci_departed) {
        hook(controller_index, hub_address, hub_port);
    }
}

#[allow(dead_code)] // EHCI enumeration is x86-64 only
pub fn ehci_root_departed_all(controller_index: usize, port: u8) {
    for hook in CLASSES.iter().filter_map(|class| class.ehci_root_departed) {
        hook(controller_index, port);
    }
}
