//! The `midi` shell command both kernels share (USB-MIDI devices: `usb_midi.rs`).

/// `midi` lists the USB-MIDI devices; `midi monitor` prints what they send until Escape; `midi send HEX...` sends one
/// message (`midi send 90 3c 64` is note C4 on) to the first device, `midi send N HEX...` to device N.
pub fn command<'a>(words: &mut impl Iterator<Item = &'a str>) {
    match words.next() {
        None => {
            crate::usb_midi::describe_devices(|arguments| crate::println!("{}", arguments));
            if crate::usb_midi::count() == 0 {
                crate::println!("midi: no USB MIDI device (`usb rescan` looks again).");
            }
        }
        Some("monitor") => {
            if crate::usb_midi::count() == 0 {
                crate::println!("midi: no USB MIDI device (`usb rescan` looks again).");
                return;
            }
            crate::println!("midi: showing what the devices send; Escape stops.");
            crate::netfs::blocking(true);
            while !crate::netfs::break_requested() {
                imp::poll();
                crate::storage::pet();
                crate::netfs::drain(|_, _| {}); // a `term net` client's Escape (and our output to it) goes through here
                while let Some((device, cable, bytes, count)) = crate::usb_midi::take() {
                    crate::println!("midi {} cable {cable}: {}", device + 1, crate::usb_midi::Describe(&bytes[..count]));
                }
                core::hint::spin_loop();
            }
            crate::netfs::blocking(false);
        }
        Some("send") => {
            let mut numbers = [0u8; 4];
            let mut count = 0;
            let mut device = 0;
            let mut first = true;
            for word in words {
                // A leading decimal number below 10 followed by more words is a device number only when written `#N`.
                if first && word.starts_with('#') {
                    device = word[1..].parse::<usize>().unwrap_or(1).saturating_sub(1);
                    first = false;
                    continue;
                }
                first = false;
                match u8::from_str_radix(word.trim_start_matches("0x"), 16) {
                    Ok(byte) if count < numbers.len() => {
                        numbers[count] = byte;
                        count += 1;
                    }
                    _ => {
                        crate::println!("midi send: expected hex bytes, at most 3 (got {word:?}).");
                        return;
                    }
                }
            }
            match crate::usb_midi::send(device, &numbers[..count]) {
                Ok(()) => crate::println!("midi: sent {}.", crate::usb_midi::Describe(&numbers[..count])),
                Err(why) => crate::println!("midi send: {why}."),
            }
        }
        Some(_) => crate::println!("Usage: midi | midi monitor | midi send [#N] HEX HEX [HEX]"),
    }
}

/// One pass over the USB controllers' event rings (what a blocking monitor loop calls).
pub fn poll_usb() {
    imp::poll();
}

#[cfg(target_arch = "x86_64")]
mod imp {
    pub fn poll() {
        crate::usb_hid::poll();
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    pub fn poll() {
        crate::usb_kbd::poll();
    }
}
