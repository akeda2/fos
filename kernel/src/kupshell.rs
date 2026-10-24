//! The `kup` command both shells share: `kup serial [BAUD]` takes a kernel image from `tools/send_kernel_serial.py` over
//! the console UART into the inactive A/B slot and restarts to try it (the loader falls back unless the new kernel
//! confirms). The protocol is `kup_serial.rs`; what differs (which UART, staging memory, changing the baud rate,
//! the reset) is in `imp`. The network KUP is `netservice.rs` (Pi) and `term.rs` (x86), and needs no command.

pub fn command(rest: &str) {
    let mut words = rest.split_whitespace();
    if words.next() != Some("serial") {
        return crate::println!("Usage: kup serial [BAUD]   (the sender is tools/send_kernel_serial.py)");
    }
    let speed = words.next().unwrap_or("");
    let baud = if speed.is_empty() { 0 } else { speed.parse::<u32>().unwrap_or(0) };
    if !speed.is_empty() && !(9_600..=3_000_000).contains(&baud) {
        return crate::println!("kup serial [BAUD]: BAUD is 9600 to 3000000 (the sender must be told the same).");
    }
    let ready = || {
        if baud == 0 {
            crate::println!("KUP serial: ready (the sender starts within 30 s).");
        } else {
            crate::println!("KUP serial: ready at {baud} baud (the sender switches too and starts within 30 s).");
        }
    };
    let Some(result) = imp::serve(baud, &ready) else { return };
    match result {
        Ok(slot) => {
            crate::println!("KUP serial: image verified and written to slot {slot} (pending); restarting to try it.");
            imp::restart();
        }
        Err(message) => crate::println!("KUP serial: {message}; nothing was written{}.", imp::dropped_note()),
    }
}

#[cfg(target_arch = "x86_64")]
mod imp {
    use crate::{term, timer};

    struct Com1;

    impl crate::kup_serial::Link for Com1 {
        fn read_byte(&mut self) -> Option<u8> {
            crate::serial::read_byte()
        }
        fn write_byte(&mut self, byte: u8) {
            crate::serial::write_byte(byte);
        }
        fn micros(&self) -> u64 {
            timer::micros()
        }
    }

    /// `None`: it could not start (already said why). COM1 keeps its speed: no baud change here yet.
    pub fn serve(baud: u32, ready: &dyn Fn()) -> Option<Result<u8, &'static str>> {
        if baud != 0 {
            crate::println!("kup serial: COM1 keeps its speed on this kernel (give no BAUD).");
            return None;
        }
        let Some(staging) = crate::tape::staging() else {
            crate::println!("kup: no memory for the image.");
            return None;
        };
        ready();
        term::flush_output();
        Some(crate::kup_serial::serve(&mut Com1, staging))
    }

    pub fn dropped_note() -> Dropped {
        Dropped(crate::serial::RX_DROPPED.load(core::sync::atomic::Ordering::Relaxed))
    }

    pub fn restart() -> ! {
        term::flush_output();
        timer::busy_wait_ms(200);
        crate::debug::mark_clean_stop();
        crate::power::reset(crate::power::runtime_services())
    }

    pub struct Dropped(usize);

    impl core::fmt::Display for Dropped {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            write!(f, " (COM1 bytes dropped: {})", self.0)
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use crate::arch::aarch64::pl011;
    use crate::{memory, timer};

    pub fn serve(baud: u32, ready: &dyn Fn()) -> Option<Result<u8, &'static str>> {
        const STAGE_BYTES: usize = 4 * 1024 * 1024;
        let Some(staging) = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None) else {
            crate::println!("kup: no memory for the image.");
            return None;
        };
        let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, STAGE_BYTES) };
        ready();
        if baud != 0 {
            pl011::drain();
            timer::busy_wait_ms(50);
            if !pl011::set_baud(baud) {
                memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
                crate::println!("kup serial: that speed is not possible with this UART clock.");
                return None;
            }
        }
        let result = crate::kup_serial::serve(&mut crate::shell::Console, buffer);
        if baud != 0 {
            pl011::drain();
            pl011::set_baud(0);
        }
        memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
        Some(result)
    }

    pub fn dropped_note() -> &'static str {
        ""
    }

    pub fn restart() -> ! {
        timer::busy_wait_ms(200);
        crate::arch::aarch64::power::reset()
    }
}
