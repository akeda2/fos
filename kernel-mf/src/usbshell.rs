//! The `usb` command of the server shell, as in the embedded kernel: `usb` lists what is plugged in, `usb rescan` looks again
//! (a stick that came back gets its old disk number, a new keyboard or stick is picked up). Hot-unplug itself needs no command: the
//! drivers notice a vanished device on the next transfer or rescan.

use kernel_core::{usb_hid, usb_serial, usb_hub};

/// Same wrapper as `audioshell::command`: the drivers wait on timer ticks (interrupts on), on CPU 0, under the USB lock.
pub fn command(words: core::str::SplitWhitespace) {
    crate::sched::pin_current(0);
    let _usb = crate::usb::lock();
    let was_on = crate::cpu::interrupts_save_and_disable();
    crate::cpu::interrupts_enable();
    run(words);
    if !was_on {
        crate::cpu::interrupts_disable();
    }
}

fn run(mut words: core::str::SplitWhitespace) {
    match words.next() {
        None => {
            usb_hub::describe_xhci(&|arguments| println!("{}", arguments));
            usb_hid::describe_other_controllers(|arguments| println!("{}", arguments));
            usb_serial::describe();
            println!("{} USB keyboard(s), {} reports so far", usb_hid::keyboard_count(), usb_hid::reports_seen());
        }
        Some("rescan") => println!("{} new device(s) (keyboards or disks).", usb_hid::probe_all()),
        Some(_) => println!("Usage: usb [rescan]"),
    }
}
