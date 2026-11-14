//! The crash record on aarch64: a small text file `crash` on the boot volume, as the embedded kernel does (`kernel/src/crash.rs`).
//! The Pi's firmware clears RAM across a reset, so the x86 page of RAM would not survive. The fatal handler writes the file when
//! the disk lock is free (a fault in the middle of a disk operation must not write: it could damage the file system) and the next
//! boot prints and deletes it.

use crate::trap::Frame;
use core::fmt::Write;

/// The record's "vector" for a watchdog reset (the same number as on x86 and in the embedded kernel).
pub const WATCHDOG_VECTOR: u64 = 240;

pub fn init() {
    println!("Crash record: the file `crash` on the boot volume (the Pi's firmware clears RAM across a reset).");
}

struct Text {
    bytes: [u8; 256],
    length: usize,
}

impl Write for Text {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let take = text.len().min(self.bytes.len() - self.length);
        self.bytes[self.length..self.length + take].copy_from_slice(&text.as_bytes()[..take]);
        self.length += take;
        Ok(())
    }
}

/// Writes the record for a fatal exception: no allocation, and nothing when the disk lock is taken.
pub fn record(kind: u64, esr: u64, elr: u64, far: u64) {
    let Some(guard) = crate::tapefs::try_lock() else {
        println!("(crash record not written: the disk was busy)");
        return;
    };
    // Dropping the guard would wake sleepers through the scheduler lock, which the crashed code may hold: the machine stops or
    // resets next anyway.
    core::mem::forget(guard);
    let Some(volume) = crate::storage::boot_volume() else { return };
    let mut text = Text { bytes: [0; 256], length: 0 };
    if kind == WATCHDOG_VECTOR {
        let _ = writeln!(text, "watchdog: no progress, the CPU was at {elr:#018x}");
    } else {
        let _ = writeln!(text, "exception {kind}: ESR {esr:#010x} ELR {elr:#018x} FAR {far:#018x}");
    }
    let _ = writeln!(text, "slot {}, {} ticks after boot", crate::slots::boot_slot(), crate::arch::ticks());
    if volume.write(b"crash", &text.bytes[..text.length]).is_ok() {
        println!("Crash record written to the file `crash`.");
    }
}

pub fn record_crash(frame: &Frame) {
    record(frame.vector, frame.esr, frame.rip, frame.far);
}

/// At boot: prints the previous boot's crash file, if there is one, and removes it.
pub fn report_previous_crash() {
    let Some(volume) = crate::storage::boot_volume() else { return };
    let mut buffer = [0u8; 256];
    let Ok(Some(length)) = volume.read(b"crash", &mut buffer) else { return };
    println!("Previous boot crashed (file `crash`, now removed):");
    for line in core::str::from_utf8(&buffer[..length]).unwrap_or("?").lines() {
        println!("  {line}");
    }
    let _ = volume.remove(b"crash");
}
