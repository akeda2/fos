//! The kernel's own GDT. In 64-bit mode segmentation is mostly vestigial, but the
//! CPU still needs a valid code segment with the L (long mode) bit set and a data
//! segment, and the firmware's GDT lives in memory the kernel will reuse. Replacing
//! it early means nothing the kernel does can pull the descriptors out from under it.

#[repr(C, align(16))]
struct Gdt([u64; 3]);

/// 0: null. 1 (selector 0x08): 64-bit kernel code, access 0x9A (present, ring 0,
/// code, readable), flags 0xA (granularity + long mode). 2 (selector 0x10): kernel
/// data, access 0x92 (present, ring 0, data, writable).
static GDT: Gdt = Gdt([0, 0x00af_9a00_0000_ffff, 0x00cf_9200_0000_ffff]);

#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;

/// Loads the GDT and reloads every segment register: data segments with `mov`, the
/// code segment with a far return (the only way to change CS in 64-bit mode).
pub fn load() {
    let gdtr = Gdtr { limit: (core::mem::size_of::<Gdt>() - 1) as u16, base: core::ptr::addr_of!(GDT) as u64 };
    unsafe {
        core::arch::asm!(
            "lgdt [{gdtr}]",
            "push {code}",
            "lea {scratch}, [rip + 2f]",
            "push {scratch}",
            "retfq",
            "2:",
            "mov ds, {data:x}",
            "mov es, {data:x}",
            "mov ss, {data:x}",
            "xor {scratch:e}, {scratch:e}",
            "mov fs, {scratch:x}",
            "mov gs, {scratch:x}",
            gdtr = in(reg) &gdtr,
            code = in(reg) u64::from(KERNEL_CODE),
            data = in(reg) u64::from(KERNEL_DATA),
            scratch = out(reg) _,
            options(preserves_flags),
        );
    }
}
