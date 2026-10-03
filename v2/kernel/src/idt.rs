//! The interrupt descriptor table: 256 gates, every one pointing at a generated entry
//! stub (`isr_stubs.S`) that normalises the stack and calls `interrupt_dispatch`.

use crate::gdt;

#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    offset_low: u16,
    selector: u16,
    /// Bits 0-2: which IST stack to switch to (0 = stay on the current one).
    ist: u8,
    /// 0x8E: present, ring 0, 64-bit interrupt gate (interrupts disabled on entry).
    type_attributes: u8,
    offset_middle: u16,
    offset_high: u32,
    reserved: u32,
}

const EMPTY: Gate = Gate { offset_low: 0, selector: 0, ist: 0, type_attributes: 0, offset_middle: 0, offset_high: 0, reserved: 0 };

#[repr(C, align(16))]
struct Idt([Gate; 256]);

static mut IDT: Idt = Idt([EMPTY; 256]);

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

unsafe extern "C" {
    /// The first of 256 stubs, 16 bytes apart.
    static isr_0: u8;
}

fn set_gate(vector: usize, ist: u8) {
    let handler = unsafe { core::ptr::addr_of!(isr_0) as u64 } + 16 * vector as u64;
    unsafe {
        (*core::ptr::addr_of_mut!(IDT)).0[vector] = Gate {
            offset_low: handler as u16,
            selector: gdt::KERNEL_CODE,
            ist,
            type_attributes: 0x8e,
            offset_middle: (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            reserved: 0,
        };
    }
}

/// Points every vector at its stub (the double fault, vector 8, on its own IST stack)
/// and loads the table.
pub fn load() {
    for vector in 0..256 {
        set_gate(vector, if vector == 8 { gdt::DOUBLE_FAULT_IST } else { 0 });
    }
    let idtr = Idtr { limit: (core::mem::size_of::<Idt>() - 1) as u16, base: core::ptr::addr_of!(IDT) as u64 };
    unsafe { core::arch::asm!("lidt [{}]", in(reg) &idtr, options(nostack, preserves_flags)) };
}
