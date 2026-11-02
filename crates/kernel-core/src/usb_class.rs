//! The USB class drivers' registry: what the host-controller side (xHCI, EHCI, hubs, mass storage) tells every class driver
//! when a device goes away or an event nobody waited for arrives.
//!
//! The controllers and hubs live in this crate and know no class driver by name. The kernel registers what it wants at start
//! (`register`: audio, MIDI, pointer, serial on the embedded kernel; a server kernel registers none) and the one device
//! enumerator (`set_host`: it probes a device the controller addressed). Order of registration is the order of the calls.
//! Attaching a new device is done by the enumerator (the descriptors differ per class); `docs/big-kernel.md`, stage 1b.

use crate::xhci::{Controller, Trb};
use core::ptr::{addr_of, addr_of_mut};

/// The device enumerator: probes devices, owns the "what step was it at" note and releases a port's slots.
pub struct Host {
    pub set_step: fn(&'static str),
    pub step: fn() -> &'static str,
    pub release_port: fn(&mut Controller, u8, Option<u32>),
    pub probe_device: fn(usize, &mut Controller, crate::xhci::Device) -> Result<bool, crate::xhci::Error>,
    /// An xHCI event nobody waited for (keyboard reports, port changes, class driver traffic).
    pub background_event: fn(&mut Controller, &Trb),
}

static mut HOST: Option<&'static Host> = None;

pub fn set_host(host: &'static Host) {
    unsafe { *addr_of_mut!(HOST) = Some(host) };
}

fn host() -> Option<&'static Host> {
    unsafe { *addr_of!(HOST) }
}

pub fn set_step(step: &'static str) {
    if let Some(host) = host() {
        (host.set_step)(step);
    }
}

pub fn step() -> &'static str {
    host().map_or("", |host| (host.step)())
}

pub fn release_port(controller: &mut Controller, port: u8, route: Option<u32>) {
    if let Some(host) = host() {
        (host.release_port)(controller, port, route);
    }
}

pub fn probe_device(controller_index: usize, controller: &mut Controller, device: crate::xhci::Device) -> Result<bool, crate::xhci::Error> {
    match host() {
        Some(host) => (host.probe_device)(controller_index, controller, device),
        None => Err(crate::xhci::Error::Unsupported),
    }
}

pub fn host_background_event(controller: &mut Controller, event: &Trb) {
    if let Some(host) = host() {
        (host.background_event)(controller, event);
    }
}

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
    /// An xHCI event in interrupt context, before it is queued; true when this driver consumed it (audio refills here).
    pub irq_event: Option<fn(usize, &Trb) -> bool>,
    /// An EHCI interrupt with transfer completion, with the current frame (audio refills here).
    pub ehci_event: Option<fn(usize, usize)>,
}

pub const NONE: Class = Class {
    release: None,
    background_event: None,
    xhci_departed: None,
    ehci_departed: None,
    ehci_root_departed: None,
    irq_event: None,
    ehci_event: None,
};

const MAX_CLASSES: usize = 8;
static mut CLASSES: [Option<&'static Class>; MAX_CLASSES] = [None; MAX_CLASSES];

/// Adds a class driver (start-up only, before any controller runs). A ninth is a bug and says so.
pub fn register(class: &'static Class) {
    let table = unsafe { &mut *addr_of_mut!(CLASSES) };
    match table.iter_mut().find(|slot| slot.is_none()) {
        Some(slot) => *slot = Some(class),
        None => crate::println!("USB: too many class drivers (max {MAX_CLASSES})."),
    }
}

fn classes() -> impl Iterator<Item = &'static Class> {
    unsafe { (*addr_of!(CLASSES)).iter().filter_map(|class| *class) }
}

pub fn release_all(controller: &mut Controller, port: u8, route: Option<u32>) {
    for hook in classes().filter_map(|class| class.release) {
        hook(controller, port, route);
    }
}

/// True when a class driver took the event.
pub fn background_event(controller: &mut Controller, event: &Trb) -> bool {
    classes().filter_map(|class| class.background_event).any(|hook| hook(controller, event))
}

pub fn xhci_departed_all(controller_index: usize, port: u8) {
    for hook in classes().filter_map(|class| class.xhci_departed) {
        hook(controller_index, port);
    }
}

#[allow(dead_code)] // EHCI enumeration is x86-64 only
pub fn ehci_departed_all(controller_index: usize, hub_address: u8, hub_port: u8) {
    for hook in classes().filter_map(|class| class.ehci_departed) {
        hook(controller_index, hub_address, hub_port);
    }
}

#[allow(dead_code)] // EHCI enumeration is x86-64 only
pub fn ehci_root_departed_all(controller_index: usize, port: u8) {
    for hook in classes().filter_map(|class| class.ehci_root_departed) {
        hook(controller_index, port);
    }
}

/// Interrupt context: true when a class driver consumed the event.
pub fn irq_event(controller_index: usize, event: &Trb) -> bool {
    classes().filter_map(|class| class.irq_event).any(|hook| hook(controller_index, event))
}

#[allow(dead_code)] // EHCI is x86-64 only
pub fn ehci_event(controller_index: usize, frame: usize) {
    for hook in classes().filter_map(|class| class.ehci_event) {
        hook(controller_index, frame);
    }
}
