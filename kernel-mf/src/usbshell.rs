//! The `usb` command of the server shell, as in the embedded kernel: `usb` lists what is plugged in, `usb rescan` looks again
//! (a stick that came back gets its old disk number, a new keyboard or stick is picked up). Hot-unplug itself needs no command: the
//! drivers notice a vanished device on the next transfer or rescan.

#[cfg(target_arch = "x86_64")]
use kernel_core::usb_hid;
#[cfg(target_arch = "aarch64")]
use kernel_core::usb_kbd as usb_hid;
use kernel_core::{usb_hub, usb_serial};

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
            #[cfg(target_arch = "x86_64")]
            usb_hid::describe_other_controllers(|arguments| println!("{}", arguments));
            usb_serial::describe();
            println!("{} USB keyboard(s), {} reports so far", usb_hid::keyboard_count(), usb_hid::reports_seen());
        }
        Some("rescan") => println!("{} new device(s) (keyboards or disks).", usb_hid::probe_all()),
        Some(_) => println!("Usage: usb [rescan]"),
    }
}

/// `touch` lists the touch panels; `touch monitor [SECONDS]` prints each finger event for that long (default 10, at most 120):
/// the embedded kernel's monitor runs until Escape, which the server shell's input path does not offer a command loop yet.
pub fn touch(words: core::str::SplitWhitespace) {
    crate::sched::pin_current(0);
    let _usb = crate::usb::lock();
    let was_on = crate::cpu::interrupts_save_and_disable();
    crate::cpu::interrupts_enable();
    touch_run(words);
    if !was_on {
        crate::cpu::interrupts_disable();
    }
}

fn touch_run(mut words: core::str::SplitWhitespace) {
    use kernel_core::usb_pointer as pointer;
    match words.next() {
        None => pointer::list_command(),
        Some("monitor") => {
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(10).clamp(1, 120);
            if pointer::count() == 0 {
                return println!("touch: no touch panel (`usb rescan` looks again).");
            }
            println!("touch: showing finger events for {seconds} s.");
            let end = crate::time::micros() + seconds * 1_000_000;
            while crate::time::micros() < end {
                crate::debug::pet();
                kernel_core::usb_class::poll();
                while let Some(event) = pointer::take() {
                    let (x, y) = pointer::range(usize::from(event.panel)).unwrap_or((0, 0));
                    println!("touch {} finger {} {} at {},{} (of {x},{y})", event.panel + 1, event.id, if event.down { "down/moved" } else { "up" }, event.x, event.y);
                }
                core::hint::spin_loop();
            }
        }
        Some(_) => println!("Usage: touch | touch monitor [SECONDS]"),
    }
}
