//! A small boot log for `dmesg`: every byte the kernel prints is also kept here (the newest 32 KiB).

const CAPACITY: usize = 32 * 1024;
static mut LOG: [u8; CAPACITY] = [0; CAPACITY];
static mut LENGTH: usize = 0;

/// Appends `text`; once the buffer is full the oldest half is dropped (up to a
/// line end), so `dmesg` always ends with the latest messages.
pub fn push(text: &str) {
    unsafe {
        let log = &mut *core::ptr::addr_of_mut!(LOG);
        for &byte in text.as_bytes() {
            if LENGTH == CAPACITY {
                let mut cut = CAPACITY / 2;
                while cut < CAPACITY && log[cut - 1] != b'\n' {
                    cut += 1;
                }
                log.copy_within(cut.., 0);
                LENGTH = CAPACITY - cut;
            }
            log[LENGTH] = byte;
            LENGTH += 1;
        }
    }
}

/// Everything logged so far.
pub fn contents() -> &'static str {
    unsafe { core::str::from_utf8(core::slice::from_raw_parts(core::ptr::addr_of!(LOG) as *const u8, LENGTH)).unwrap_or("") }
}
