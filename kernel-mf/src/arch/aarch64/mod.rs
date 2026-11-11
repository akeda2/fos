//! Everything the server kernel does differently on aarch64 (Raspberry Pi 4, QEMU `virt`): the entry, exception vectors, GIC,
//! generic timer, page-table hardware and (to come) PSCI SMP start. It offers the same hooks as `arch/x86_64`; the shared
//! kernel (`main.rs`) calls them. The CPU-specific files of the embedded aarch64 kernel are used as they are (`#[path]`), so the
//! two cannot drift.
//!
//! Memory is identity-mapped (`vm::direct` adds 0): the kernel image, RAM and, on demand, device registers all sit at their
//! physical addresses in TTBR0, as in the embedded kernel. User address spaces will live above 512 GiB in the same TTBR0
//! tables. These shims (`vm`, `memory`, `crash`, `slots`, `trap`, `debug`) stand in for the modules the shared kernel gets
//! from its own files on x86 and shrink as those become portable.

use fosfor_abi::BootInfo;
use crate::boot;

// The embedded kernel's aarch64 files, shared. They name `crate::cpu`, `crate::gic`, `crate::paging`, `crate::timer`,
// `crate::crash` and `crate::memory`, which are provided here.
#[path = "../../../../kernel/src/arch/aarch64/acpi.rs"]
pub mod acpi;
#[path = "../../../../kernel/src/arch/aarch64/early.rs"]
pub mod early;
#[path = "../../../../kernel/src/arch/aarch64/exceptions.rs"]
pub mod exceptions;
#[path = "../../../../kernel/src/arch/aarch64/gic.rs"]
pub mod gic;
#[path = "../../../../kernel/src/arch/aarch64/paging.rs"]
pub mod paging;
#[path = "../../../../kernel/src/arch/aarch64/pl011.rs"]
pub mod pl011;
#[path = "../../../../kernel/src/arch/aarch64/timer.rs"]
pub mod timer;
#[path = "../../../../kernel/src/arch/aarch64/cpu.rs"]
mod cpu_arch;

pub mod cpu {
    pub use super::cpu_arch::*;

    /// A lock may be waited for about 5 s (the generic counter runs at 19.2-62.5 MHz).
    pub const DEADLOCK_CYCLES: u64 = 300_000_000;

    /// The free-running counter the spin-lock deadlock detector uses.
    pub fn cycles() -> u64 {
        crate::timer::counter()
    }
}

/// Identity map: a physical address is the pointer to it.
pub mod vm {
    pub const DIRECT_BASE: u64 = 0;
    pub fn direct(physical: u64) -> u64 {
        physical
    }
    pub fn root() -> u64 {
        crate::paging::root()
    }
}

/// What the shared aarch64 files ask of the embedded kernel's `memory` module.
pub mod memory {
    pub const FRAME: u64 = crate::pmm::FRAME;
    pub fn allocate() -> Option<u64> {
        crate::pmm::allocate(1, None)
    }
    /// The Pi 4's PCIe bridge reaches only the first 3 GiB; reserving a pool below that is for the PCI slice.
    pub fn dma_pool(_frames: u64) {}
}

/// The crash machinery (record, reset) is not ported yet: a fatal exception is reported and the CPU parks.
pub mod crash {
    pub fn tick() {}
    pub fn fatal(_kind: u64, _esr: u64, _elr: u64, _far: u64, _always_reset: bool) -> ! {
        crate::println!("*** fatal exception: halting.");
        crate::halt()
    }
}

pub mod slots {
    pub fn mark_clean_stop() {}
}

pub mod trap {
    /// What a fatal path reports; the aarch64 vectors fill it from ESR/ELR/FAR when the crash record is ported.
    pub struct Frame {
        pub vector: u64,
        pub rip: u64,
    }
    pub fn fatal_reset(_frame: &Frame) -> ! {
        crate::println!("*** fatal: halting.");
        crate::halt()
    }
}

pub mod debug {
    pub const PANIC_VECTOR: u64 = 0xff00;
    pub const DEADLOCK_VECTOR: u64 = 0xff01;
}


const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;
const PI4_PL011: u64 = 0xfe20_1000;

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    _start:
        msr daifset, #0xf              // debug, SError, IRQ, FIQ masked
        adrp x1, __stack_top
        add x1, x1, :lo12:__stack_top
        mov sp, x1
        mov x29, xzr
        bl kmain                       // x0 still holds the BootInfo pointer
    1:  wfe
        b 1b
    "#
);

pub fn halt() -> ! {
    cpu::halt()
}

pub fn wait_for_interrupt() {
    cpu::sleep()
}

pub fn ticks() -> u64 {
    gic::irq_ticks()
}

pub fn micros() -> u64 {
    timer::micros()
}

pub fn busy_wait_ms(ms: u64) {
    let start = micros();
    while micros() - start < ms * 1000 {
        cpu::sleep();
    }
}

/// Second half of the banner line.
pub struct RunningOn;
impl core::fmt::Display for RunningOn {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "running from the UEFI loader at EL{}.", exceptions::current_el())
    }
}
pub fn running_on() -> RunningOn {
    RunningOn
}

/// The vectors, then the console where the firmware's tables say (QEMU `virt` and the Pi 4 differ), set before the first line.
pub fn early(info: &BootInfo) {
    exceptions::init();
    if info.is_valid() {
        if let Some(base) = acpi::spcr_uart(info.acpi_rsdp) {
            pl011::set_base(base);
        } else if acpi::madt_gic(info.acpi_rsdp).is_some_and(|(distributor, _)| distributor == PI4_GIC_DISTRIBUTOR) {
            pl011::set_base(PI4_PL011);
        }
        acpi::remember(info.acpi_rsdp);
        if let Some((distributor, cpu_interface)) = acpi::madt_gic(info.acpi_rsdp) {
            gic::set_base(distributor, cpu_interface);
        }
    }
}

/// The ACPI tables stay where the firmware left them, so their pages are not handed out.
pub fn reserved_extra(info: &BootInfo) -> (u64, u64) {
    (info.acpi_rsdp & !0xfff, 16384)
}

/// Our own page tables; the loader's BootInfo page stays readable (identity map).
pub fn init_memory(_machine: &boot::Machine, info: &BootInfo) -> &'static BootInfo {
    paging::build_and_load(info, &info.framebuffer);
    paging::map_device(pl011::base(), 4096);
    println!("Page tables: loaded; RAM no-execute in 2 MiB blocks, kernel image W^X, page 0 and the stack guard unmapped.");
    unsafe { &*(info as *const BootInfo) }
}

pub fn init_interrupts() {
    gic::init();
}

/// Interrupts on, and the timer ticking; the century register is an x86 RTC detail.
pub fn init_machine(_machine: &boot::Machine) -> u8 {
    cpu::interrupts_enable();
    let start = timer::micros();
    while timer::micros() - start < 100_000 {
        cpu::sleep();
    }
    println!("GIC at {:#x}/{:#x}: {} timer interrupts (INTID 30) in 100 ms, expected about 10; counter {} Hz.", gic::bases().0, gic::bases().1, gic::irq_ticks(), timer::frequency());
    0
}
