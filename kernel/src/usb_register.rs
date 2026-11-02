//! The embedded kernel's USB class drivers, handed to the controller code in `kernel-core` (`usb_class`, docs/big-kernel.md
//! stage 1b). Order is the order the calls were made in before the registry: audio, MIDI, pointer, then serial for events
//! and releases. A kernel without these drivers (the server one) registers none and still runs the controllers, hubs and disks.

use crate::usb_class::{self, Class, Host, NONE};

#[cfg(target_arch = "x86_64")]
use crate::usb_hid as enumerator;
#[cfg(target_arch = "aarch64")]
use crate::usb_kbd as enumerator;

static HOST: Host = Host {
    set_step: enumerator::set_step,
    step: enumerator::step,
    release_port: enumerator::release_port,
    probe_device: |index, controller, device| enumerator::probe_device(index, controller, device),
    background_event: enumerator::background_event,
};

static AUDIO: Class = Class {
    ehci_departed: Some(crate::usb_audio::ehci_departed),
    irq_event: Some(crate::usb_audio::irq_event),
    ehci_event: Some(crate::usb_audio::ehci_event),
    ..NONE
};

static MIDI: Class = Class {
    release: Some(crate::usb_midi::release),
    background_event: Some(crate::usb_midi::background_event),
    ..NONE
};

static POINTER: Class = Class {
    release: Some(crate::usb_pointer::release),
    background_event: Some(crate::usb_pointer::background_event),
    ..NONE
};

static SERIAL: Class = Class {
    background_event: Some(crate::usb_serial::background_event),
    xhci_departed: Some(crate::usb_serial::xhci_departed),
    ehci_departed: Some(crate::usb_serial::ehci_departed),
    ehci_root_departed: Some(crate::usb_serial::ehci_root_departed),
    ..NONE
};

pub fn install() {
    usb_class::set_host(&HOST);
    for class in [&AUDIO, &MIDI, &POINTER, &SERIAL] {
        usb_class::register(class);
    }
}
