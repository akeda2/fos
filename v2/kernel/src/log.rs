//! A small boot log: every byte the kernel prints is also kept here, so text written
//! before the framebuffer console exists (everything up to the page-table switch)
//! can be replayed onto the screen once it does. The same idea as v1's `dmesg`.

const CAPACITY: usize = 16 * 1024;
static mut LOG: [u8; CAPACITY] = [0; CAPACITY];
static mut LENGTH: usize = 0;

/// Appends `text`; once the buffer is full, further text is dropped (the serial
/// port still has it).
pub fn push(text: &str) {
    unsafe {
        let log = &mut *core::ptr::addr_of_mut!(LOG);
        for &byte in text.as_bytes() {
            if LENGTH < CAPACITY {
                log[LENGTH] = byte;
                LENGTH += 1;
            }
        }
    }
}

/// Everything logged so far.
pub fn contents() -> &'static str {
    unsafe { core::str::from_utf8(core::slice::from_raw_parts(core::ptr::addr_of!(LOG) as *const u8, LENGTH)).unwrap_or("") }
}
