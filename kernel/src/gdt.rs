//! The kernel's GDT and TSS. In 64-bit mode segmentation is mostly vestigial, but the
//! CPU still needs a code segment with the L (long mode) bit set, a data segment, and
//! a task-state segment whose Interrupt Stack Table gives the double-fault handler a
//! stack of its own: when the *normal* stack is the problem (an overflow into the
//! unmapped guard page), the handler must not run on it.

const GDT_ENTRIES: usize = 5;

#[repr(C, align(16))]
struct Gdt([u64; GDT_ENTRIES]);

/// 0: null. 1 (selector 0x08): 64-bit kernel code, access 0x9A (present, ring 0,
/// code, readable), flags 0xA (granularity + long mode). 2 (selector 0x10): kernel
/// data, access 0x92. 3-4 (selector 0x18): the TSS descriptor, 16 bytes, filled in
/// at run time because it holds the TSS's address.
static mut GDT: Gdt = Gdt([0, 0x00af_9a00_0000_ffff, 0x00cf_9200_0000_ffff, 0, 0]);

/// The 64-bit task-state segment: only the IST pointers matter here.
#[repr(C, packed)]
struct Tss {
    reserved0: u32,
    rsp: [u64; 3],
    reserved1: u64,
    ist: [u64; 7],
    reserved2: u64,
    reserved3: u16,
    io_map_base: u16,
}

static mut TSS: Tss = Tss { reserved0: 0, rsp: [0; 3], reserved1: 0, ist: [0; 7], reserved2: 0, reserved3: 0, io_map_base: 104 };

#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const TSS_SELECTOR: u16 = 0x18;
/// IST slot used by the double-fault gate (1-based, as the IDT entry counts it).
pub const DOUBLE_FAULT_IST: u8 = 1;

/// Installs the GDT and TSS: `double_fault_stack_top` becomes IST1. Reloads every
/// segment register (data ones with `mov`, CS with a far return, the only way to
/// change it in 64-bit mode) and loads the task register.
pub fn load(double_fault_stack_top: u64) {
    unsafe {
        let tss = core::ptr::addr_of_mut!(TSS);
        (*tss).ist[usize::from(DOUBLE_FAULT_IST) - 1] = double_fault_stack_top;
        let base = tss as u64;
        let limit = (core::mem::size_of::<Tss>() - 1) as u64;
        let gdt = core::ptr::addr_of_mut!(GDT);
        // 64-bit TSS descriptor: limit, base split over three fields, type 0x9
        // (available 64-bit TSS), present; the second quadword holds base bits 63..32.
        (*gdt).0[3] = (limit & 0xffff) | (base & 0xffff) << 16 | ((base >> 16) & 0xff) << 32 | 0x89 << 40
            | ((limit >> 16) & 0xf) << 48 | ((base >> 24) & 0xff) << 56;
        (*gdt).0[4] = base >> 32;
        let gdtr = Gdtr { limit: (core::mem::size_of::<Gdt>() - 1) as u16, base: gdt as u64 };
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
            "ltr {tss:x}",
            gdtr = in(reg) &gdtr,
            code = in(reg) u64::from(KERNEL_CODE),
            data = in(reg) u64::from(KERNEL_DATA),
            tss = in(reg) u64::from(TSS_SELECTOR),
            scratch = out(reg) _,
            options(preserves_flags),
        );
    }
}
