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
    with_usb(|| touch_run(words));
}

/// Runs `f` as the `usb` command does: on CPU 0, under the USB lock, with interrupts on (the controllers are polled).
fn with_usb(f: impl FnOnce()) {
    crate::sched::pin_current(0);
    let _usb = crate::usb::lock();
    let was_on = crate::cpu::interrupts_save_and_disable();
    crate::cpu::interrupts_enable();
    f();
    if !was_on {
        crate::cpu::interrupts_disable();
    }
}

/// `midi` lists the USB-MIDI devices; `midi monitor [SECONDS]` prints what they send (10 s by default, 120 at most: the shell
/// has no Escape for a blocking loop); `midi send [#N] HEX HEX [HEX]` sends one message (`90 3c 64` is note C4 on).
pub fn midi(words: core::str::SplitWhitespace) {
    with_usb(|| midi_run(words));
}

fn midi_run(mut words: core::str::SplitWhitespace) {
    use kernel_core::usb_midi as midi;
    let none = || println!("midi: no USB MIDI device (`usb rescan` looks again).");
    match words.next() {
        None => {
            midi::describe_devices(|arguments| println!("{}", arguments));
            if midi::count() == 0 {
                none();
            }
        }
        Some("monitor") => {
            let seconds = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(10).clamp(1, 120);
            if midi::count() == 0 {
                return none();
            }
            println!("midi: showing what the devices send for {seconds} s.");
            let end = crate::time::micros() + seconds * 1_000_000;
            while crate::time::micros() < end {
                crate::debug::pet();
                kernel_core::usb_class::poll();
                while let Some((device, cable, bytes, count)) = midi::take() {
                    println!("midi {} cable {cable}: {}", device + 1, midi::Describe(&bytes[..count]));
                }
                core::hint::spin_loop();
            }
        }
        Some("send") => {
            let mut numbers = [0u8; 4];
            let (mut count, mut device, mut first) = (0, 0, true);
            for word in words {
                if first && word.starts_with('#') {
                    device = word[1..].parse::<usize>().unwrap_or(1).saturating_sub(1);
                    first = false;
                    continue;
                }
                first = false;
                match u8::from_str_radix(word.trim_start_matches("0x"), 16) {
                    Ok(byte) if count < 3 => {
                        numbers[count] = byte;
                        count += 1;
                    }
                    _ => return println!("midi send: expected hex bytes, at most 3 (got {word:?})."),
                }
            }
            match midi::send(device, &numbers[..count]) {
                Ok(()) => println!("midi: sent {}.", midi::Describe(&numbers[..count])),
                Err(why) => println!("midi send: {why}."),
            }
        }
        Some(_) => println!("Usage: midi | midi monitor [SECONDS] | midi send [#N] HEX HEX [HEX]"),
    }
}

fn touch_run(mut words: core::str::SplitWhitespace) {
    use kernel_core::usb_pointer as pointer;
    match words.next() {
        None => {
            crate::ps2::deliver_mouse();
            pointer::list_command()
        }
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
                crate::ps2::poll(); // the shell runs on the service thread, which polls the 8042
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
