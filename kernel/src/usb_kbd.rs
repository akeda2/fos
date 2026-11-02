//! The Pi's keyboard queue and layout. The USB boot-keyboard enumerator is `kernel_core::usb_kbd`; it types into this queue
//! through `usb_env` (registered in `platform.rs`). The x86 kernel has `keyboard.rs` for this.

pub use kernel_core::usb_kbd::*;

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use fosfor_abi::keymap::Layout;

const QUEUE: usize = 64;

static mut KEYS: [u8; QUEUE] = [0; QUEUE];
static SWEDISH: AtomicBool = AtomicBool::new(false);
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);

pub fn push(byte: u8) {
    let head = HEAD.load(Ordering::Relaxed);
    let next = (head + 1) % QUEUE;
    if next != TAIL.load(Ordering::Acquire) {
        unsafe { (*core::ptr::addr_of_mut!(KEYS))[head] = byte };
        HEAD.store(next, Ordering::Release);
    }
}

/// The layout a `keymap` word names (the x86 `keyboard::layout_named`).
pub fn layout_named(name: &str) -> Option<Layout> {
    match name {
        "us" => Some(Layout::Us),
        "sv" | "se" | "fi" => Some(Layout::Swedish),
        _ => None,
    }
}

/// Chooses what the keys type; US until told otherwise (`keymap` command, `settings` file).
pub fn set_layout(layout: Layout) {
    SWEDISH.store(layout == Layout::Swedish, Ordering::Relaxed);
}

pub fn layout() -> Layout {
    if SWEDISH.load(Ordering::Relaxed) { Layout::Swedish } else { Layout::Us }
}

/// The next typed byte, if one is waiting.
pub fn pop() -> Option<u8> {
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        return None;
    }
    let byte = unsafe { (*core::ptr::addr_of!(KEYS))[tail] };
    TAIL.store((tail + 1) % QUEUE, Ordering::Release);
    Some(byte)
}
