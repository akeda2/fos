//! The `usb` command both shells share: `usb` lists the controllers and what is plugged in, `usb rescan` looks again.
//!
//! The xHCI listing (controllers, ports, hubs) is one piece of code: both kernels have `xhci.rs` and `usb_hub.rs`.
//! What only one kernel has (x86: EHCI, OHCI, MIDI, serial adapters, the boot-disk check; the Pi: nothing more) is in `imp`.
//! x86-only diagnostics (`usb try`, `usb abort-test`) stay in `term.rs`.

use crate::{usb_hub, xhci};

pub fn command(rest: &str) {
    match rest.split_whitespace().next() {
        None => list(),
        Some("rescan") => {
            crate::println!("{} new device(s) (keyboards or disks).", imp::rescan());
            imp::after_rescan();
        }
        Some(_) => crate::println!("Usage: usb [rescan]"),
    }
}

fn list() {
    describe_xhci(&|arguments| crate::println!("{}", arguments));
    imp::describe_more();
    crate::println!("{}", imp::keyboards());
}

/// Every xHCI controller: its ports with something plugged in, and the hubs below them.
pub fn describe_xhci(print: &impl Fn(core::fmt::Arguments)) {
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
        usb_hub::describe(index, controller, print);
    }
}

/// The closing line of the list, printable without allocating.
pub struct Keyboards {
    pub count: usize,
    pub reports: u64,
}

impl core::fmt::Display for Keyboards {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{} USB keyboard(s), {} reports so far", self.count, self.reports)
    }
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use super::Keyboards;
    use crate::usb_hid;

    pub fn rescan() -> usize {
        usb_hid::probe_all()
    }

    pub fn after_rescan() {
        if crate::storage::boot_disk().is_none() && crate::storage::locate_boot_disk(false) {
            crate::println!("The boot disk turned up late: `start` runs its start script (hostname, settings and debug files load at boot only).");
        }
    }

    /// EHCI, OHCI, MIDI and serial adapters.
    pub fn describe_more() {
        usb_hid::describe_other_controllers(|arguments| crate::println!("{}", arguments));
        crate::usb_serial::describe();
    }

    pub fn keyboards() -> Keyboards {
        Keyboards { count: usb_hid::keyboard_count(), reports: usb_hid::reports_seen() }
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use super::Keyboards;

    pub fn rescan() -> usize {
        crate::usb_kbd::probe_all()
    }

    pub fn after_rescan() {}

    pub fn describe_more() {
        crate::usb_serial::describe();
    }

    pub fn keyboards() -> Keyboards {
        Keyboards { count: crate::usb_kbd::keyboard_count(), reports: crate::usb_kbd::reports_seen() }
    }
}

