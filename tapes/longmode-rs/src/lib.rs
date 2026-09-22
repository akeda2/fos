//! `tape64_body` written in ordinary `#![no_std]` Rust instead of hand-
//! written assembly -- the counterpart to `../longmode.S`, both linked
//! against the same reusable `tapes/longmode64_shim.S` (see that file's
//! header comment for the full `tape64_body` contract: the shim now
//! passes `args_ptr`, `args_len`, and `report_ptr` for newer bodies, but
//! a body declaring no parameters remains valid under SysV64 and simply
//! ignores them. This crate intentionally does that -- its `tape64_body`
//! takes no arguments and returns nothing, called once the shim has
//! already entered 64-bit mode with its private GDT loaded and
//! interrupts masked, and expected to return normally with a plain
//! `ret`). See AGENTS.md's "A tape that visits 64-bit long mode and
//! comes back" section for the full design rationale behind the
//! surrounding mode-switch machinery -- none of that is repeated here.
//!
//! This does exactly what `../longmode.S`'s hand-written version does:
//! writes a fixed, distinctive 64-bit-only value (`0xdeadbeefcafebabe`
//! -- something no 32-bit `mov` could ever load in one instruction)
//! straight to VGA text memory at physical `0xb8000`, as proof that
//! 64-bit-encoded instructions genuinely ran. It cannot go through the
//! kernel's `putc` callback (see the shim's header comment on why: that
//! callback is 32-bit code, and calling it from 64-bit mode would need
//! a second, nested mode transition for one debug print), so this
//! writes directly to VGA memory instead, one `(character, attribute)`
//! `u16` cell at a time -- the same convention `tapes/vga-colors/src/
//! main.rs` uses, just recompiled for a different CPU mode instead of a
//! different algorithm. Comparing this file to `../longmode.S` line by
//! line is the point: nothing about page tables, the private GDT, the
//! mode transition itself, or register/CR0-CR4/EFER/GDTR/EFLAGS
//! preservation appears in either -- that is all `tapes/
//! longmode64_shim.S`'s job, shared unchanged by both.
#![no_std]

use core::panic::PanicInfo;

/// Physical address of the VGA text-mode framebuffer, 80x25 cells of one
/// `u16` each (low byte: ASCII character; high byte: attribute --
/// foreground/background color), identity-mapped by the shim's own page
/// tables before this function ever runs.
const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;

/// White-on-black and green-on-black VGA attribute bytes, matching
/// `tapes/vga-colors/src/main.rs`'s own attribute-byte convention
/// (`0x0f`/`0x0a`).
const ATTR_WHITE: u16 = 0x0f00;
const ATTR_GREEN: u16 = 0x0a00;

/// Writes one `(character, attribute)` cell directly to VGA text memory
/// at `column` on row 0. `unsafe` only because it's a raw MMIO write to
/// a fixed physical address with no memory-safety story beyond "this
/// address is always VGA text memory on the machines fos targets" --
/// the same trust every other tape's direct VGA access already
/// requires (see `tapes/vga-colors/src/main.rs`).
fn put_vga_cell(column: usize, byte: u8, attribute: u16) {
    unsafe {
        VGA_BUFFER.add(column).write_volatile(attribute | u16::from(byte));
    }
}

/// Writes a NUL-terminated-in-spirit (just a `&[u8]` here, no NUL needed)
/// ASCII string starting at VGA column `start`, returning the column
/// immediately after the last character written.
fn put_vga_str(start: usize, text: &[u8], attribute: u16) -> usize {
    for (index, &byte) in text.iter().enumerate() {
        put_vga_cell(start + index, byte, attribute);
    }
    start + text.len()
}

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// The one function this crate exists to provide -- see the file header
/// comment and `tapes/longmode64_shim.S` for the calling contract.
#[unsafe(no_mangle)]
pub extern "C" fn tape64_body() {
    let mut column = put_vga_str(0, b"64-BIT MODE REACHED (RUST), RAX=", ATTR_WHITE);

    // A value no 32-bit `mov` could ever load into one register in one
    // instruction -- proof this is genuinely running as compiled 64-bit
    // code, not just claiming to.
    let value: u64 = 0xdead_beef_cafe_babe;
    for shift in (0..16).rev() {
        let nibble = ((value >> (shift * 4)) & 0xf) as usize;
        put_vga_cell(column, HEX_DIGITS[nibble], ATTR_GREEN);
        column += 1;
    }
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    // Same reasoning as every other tape's panic handler (see
    // tapes/hello-rs/src/main.rs): nowhere to unwind to, so spin
    // forever. A panic here would leave the CPU in 64-bit mode with
    // tapes/longmode64_shim.S's private GDT loaded and interrupts
    // masked, never reaching its own restore sequence -- unrecoverable
    // by design, exactly like any other tape fault (see `tape_run`'s
    // doc comment in kernel/src/shell.rs).
    loop {}
}
