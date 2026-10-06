//! Full-screen tape panels: the pieces `audio-net` and the recorder share. It draws with the few ANSI sequences the
//! console and every terminal know (clear, cursor position, erase to end of line), so a panel works on the local
//! screen and over `term net` alike.
//!
//! - this file: buffered screen output (`draw!`, `at`, `present`) and the fixed-size `Text`;
//! - `meters`: a bank of programme peak meters (Nordic scale or dBFS) with peak hold, clip latch and paging.
#![no_std]

pub mod meters;

use core::fmt::Write;

/// The screen is written through a buffer and handed to the console in one call (`present`): printing a byte at a time
/// makes the console repaint its whole text grid for every byte, which is what once held a stream up for 20 ms.
static mut FRAME: [u8; 2048] = [0; 2048];
static mut FRAME_LENGTH: usize = 0;

#[doc(hidden)]
pub struct Screen;

impl Write for Screen {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            unsafe {
                if FRAME_LENGTH == 2048 {
                    present();
                }
                (*(&raw mut FRAME))[FRAME_LENGTH] = byte;
                FRAME_LENGTH += 1;
            }
        }
        Ok(())
    }
}

/// Sends what has been drawn since the last call to the console.
pub fn present() {
    unsafe {
        if FRAME_LENGTH == 0 {
            return;
        }
        let frame: &[u8; 2048] = &*(&raw const FRAME);
        let api = tape_rt::api();
        if api.version >= 2 {
            (api.puts)(frame.as_ptr(), FRAME_LENGTH);
        } else {
            for &byte in &frame[..FRAME_LENGTH] {
                (api.putc)(u32::from(byte));
            }
        }
        FRAME_LENGTH = 0;
    }
}

/// Draws text and presents the whole buffer (a cursor move from `at` goes out with the text that follows it).
#[macro_export]
macro_rules! draw {
    ($($argument:tt)*) => {{
        let _ = core::fmt::Write::write_fmt(&mut $crate::Screen, format_args!($($argument)*));
        $crate::present();
    }};
}

/// Moves the cursor (1-based); it is sent together with the next `draw!`.
pub fn at(row: usize, column: usize) {
    let _ = write!(Screen, "\x1b[{row};{column}H");
}

/// A short text kept inline (no allocation); writing past the end drops the excess.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Text<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Text<N> {
    pub const fn new() -> Self {
        Text { bytes: [0; N], length: 0 }
    }
    pub fn from(text: &str) -> Self {
        let mut result = Self::new();
        for &byte in text.as_bytes() {
            result.push(byte);
        }
        result
    }
    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
    pub fn push(&mut self, byte: u8) {
        if self.length < N {
            self.bytes[self.length] = byte;
            self.length += 1;
        }
    }
    pub fn clear(&mut self) {
        self.length = 0;
    }
    pub fn pop(&mut self) {
        self.length = self.length.saturating_sub(1);
    }
    pub fn is_empty(&self) -> bool {
        self.length == 0
    }
    pub fn len(&self) -> usize {
        self.length
    }
}

impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for &byte in text.as_bytes() {
            self.push(byte);
        }
        Ok(())
    }
}
