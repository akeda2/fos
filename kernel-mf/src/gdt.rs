//! Per-CPU GDT and TSS. In 64-bit mode segmentation is mostly vestigial, but the CPU still needs a long-mode code segment,
//! a data segment and a task-state segment whose Interrupt Stack Table gives the double-fault handler a stack of its own.
//! Every CPU has its own pair (see `percpu.rs`). Ring 3 needs a user data and a user 64-bit code segment, laid out for
//! `syscall`/`sysret` (STAR: kernel CS 0x08, SS 0x10; user SS base+8, CS base+16 with base 0x10).

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

#[repr(C, packed)]
struct Gdtr {
    limit: u16,
    base: u64,
}

/// 0: null. 1 (0x08): 64-bit kernel code. 2 (0x10): kernel data. 3 (0x18): user data (DPL 3). 4 (0x20): user 64-bit code (DPL 3).
/// 5-6 (0x28): the TSS descriptor, 16 bytes, filled in at run time because it holds the TSS's address.
#[repr(C, align(16))]
pub struct CpuGdt {
    gdt: [u64; 7],
    tss: Tss,
}

impl CpuGdt {
    pub const fn new() -> Self {
        CpuGdt {
            gdt: [0, 0x00af_9a00_0000_ffff, 0x00cf_9200_0000_ffff, 0x00cf_f200_0000_ffff, 0x00af_fa00_0000_ffff, 0, 0],
            tss: Tss { reserved0: 0, rsp: [0; 3], reserved1: 0, ist: [0; 7], reserved2: 0, reserved3: 0, io_map_base: 104 },
        }
    }
}

pub const KERNEL_CODE: u16 = 0x08;
pub const KERNEL_DATA: u16 = 0x10;
pub const USER_DATA: u16 = 0x18 | 3;
pub const USER_CODE: u16 = 0x20 | 3;
pub const TSS_SELECTOR: u16 = 0x28;
/// IST slot used by the double-fault gate (1-based, as the IDT entry counts it).
pub const DOUBLE_FAULT_IST: u8 = 1;

/// Installs `this` as the CPU's GDT and TSS: `double_fault_stack_top` becomes IST1. Reloads every segment register (CS with a
/// far return, the only way in 64-bit mode) and loads the task register.
pub fn load(this: &mut CpuGdt, double_fault_stack_top: u64) {
    this.tss.ist[usize::from(DOUBLE_FAULT_IST) - 1] = double_fault_stack_top;
    let base = &raw const this.tss as u64;
    let limit = (core::mem::size_of::<Tss>() - 1) as u64;
    // 64-bit TSS descriptor: limit, base split over three fields, type 0x9 (available 64-bit TSS), present; the second
    // quadword holds base bits 63..32.
    this.gdt[5] = (limit & 0xffff) | (base & 0xffff) << 16 | ((base >> 16) & 0xff) << 32 | 0x89 << 40 | ((limit >> 16) & 0xf) << 48 | ((base >> 24) & 0xff) << 56;
    this.gdt[6] = base >> 32;
    let gdtr = Gdtr { limit: (core::mem::size_of::<[u64; 7]>() - 1) as u16, base: &raw const this.gdt as u64 };
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

/// Where the CPU switches to on an interrupt or exception from ring 3 (the kernel stack of the thread that runs there).
pub fn set_rsp0(this: &mut CpuGdt, top: u64) {
    this.tss.rsp[0] = top;
}
