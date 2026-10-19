//! The ARM PrimeCell PL011 UART: the Pi 4's UART0 and QEMU `virt`'s console. Transmit only for now.
//! Registers (offsets): DR 0x00 (write a byte), FR 0x18 (bit 5 TXFF: transmit FIFO full, bit 3 BUSY).
//! The firmware has already set the baud rate and enabled the UART, so the kernel just writes; a bounded wait on TXFF
//! keeps a UART that never drains from hanging the kernel.

use core::sync::atomic::{AtomicU64, Ordering};

/// QEMU `virt`'s PL011. A real board's address is found from the firmware's SPCR/DT tables (the Pi 4's UART0 is
/// 0xFE201000 behind the firmware's own mapping); until that is parsed, `set_base` can override it.
pub const QEMU_VIRT_BASE: u64 = 0x0900_0000;

static BASE: AtomicU64 = AtomicU64::new(QEMU_VIRT_BASE);

#[allow(dead_code)]
pub fn set_base(base: u64) {
    BASE.store(base, Ordering::Relaxed);
}

fn put(byte: u8) {
    let base = BASE.load(Ordering::Relaxed);
    for _ in 0..100_000 {
        if unsafe { core::ptr::read_volatile((base + 0x18) as *const u32) } & 1 << 5 == 0 {
            break;
        }
    }
    unsafe { core::ptr::write_volatile(base as *mut u32, u32::from(byte)) };
}

pub struct Serial;

impl core::fmt::Write for Serial {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                put(b'\r');
            }
            put(byte);
        }
        Ok(())
    }
}
