//! Small helpers every tape with `key=value` arguments and a text status line ends up needing: `arg_value` and `parse_f32`
//! for the arguments, `Line` for a line of text built with `write!`, and `alloc_zeroed` / `alloc_slice` for memory from the
//! kernel (a static would put the whole buffer in the tape's file).

use crate::TapeApi;
use core::fmt::Write;

/// The value of `key=value` among the whitespace-separated words of `arguments`.
pub fn arg_value<'a>(arguments: &'a str, key: &str) -> Option<&'a str> {
    arguments.split_ascii_whitespace().find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

/// A decimal number with an optional sign and fraction (`-12`, `0.5`, `+3.25`). No exponent, no libm.
pub fn parse_f32(text: &str) -> Option<f32> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let mut result = whole.parse::<u32>().ok()? as f32;
    let mut scale = 0.1;
    for digit in fraction.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        result += (digit - b'0') as f32 * scale;
        scale /= 10.0;
    }
    Some(if negative { -result } else { result })
}

/// A line of text of at most `N` bytes, built with `write!`; what does not fit is dropped.
pub struct Line<const N: usize> {
    pub bytes: [u8; N],
    pub length: usize,
}

impl<const N: usize> Line<N> {
    pub const fn new() -> Self {
        Self { bytes: [0; N], length: 0 }
    }

    pub fn clear(&mut self) {
        self.length = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl<const N: usize> Default for Line<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Line<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            if self.length < N {
                self.bytes[self.length] = byte;
                self.length += 1;
            }
        }
        Ok(())
    }
}

/// A zeroed `T` in memory from the kernel (`api.alloc`), kept until the tape ends. None when the kernel has no room.
pub fn alloc_zeroed<T>(api: &TapeApi) -> Option<&'static mut T> {
    let bytes = core::mem::size_of::<T>();
    let address = (api.alloc)(bytes);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, bytes);
        Some(&mut *(address as *mut T))
    }
}

/// `count` zeroed `u32`s (a pixel buffer), from the kernel.
pub fn alloc_pixels(api: &TapeApi, count: usize) -> Option<&'static mut [u32]> {
    let address = (api.alloc)(count * 4);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, count * 4);
        Some(core::slice::from_raw_parts_mut(address as *mut u32, count))
    }
}

/// `count` zeroed bytes from the kernel.
pub fn alloc_bytes(api: &TapeApi, count: usize) -> Option<&'static mut [u8]> {
    let address = (api.alloc)(count);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, count);
        Some(core::slice::from_raw_parts_mut(address as *mut u8, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        assert_eq!(arg_value("device=2 level=-20 save", "level"), Some("-20"));
        assert_eq!(arg_value("device=2 level=-20 save", "save"), None);
        assert_eq!(arg_value("xlevel=1", "level"), None);
    }

    #[test]
    fn numbers() {
        assert_eq!(parse_f32("-12"), Some(-12.0));
        assert_eq!(parse_f32("+0.5"), Some(0.5));
        assert!((parse_f32("3.25").unwrap() - 3.25).abs() < 1e-6);
        assert_eq!(parse_f32("1e3"), None);
        assert_eq!(parse_f32(""), None);
    }

    #[test]
    fn lines_truncate() {
        let mut line = Line::<5>::new();
        let _ = write!(line, "{}-{}", 12, 3456);
        assert_eq!(line.as_str(), "12-34");
        line.clear();
        assert!(line.is_empty());
    }
}
