// The aarch64 entry of the server kernel (Raspberry Pi 4, QEMU `virt`), entered from the aarch64 UEFI loader with the same
// `BootInfo` as the embedded aarch64 kernel. It starts small and grows toward the x86-64 server kernel (docs/big-kernel.md
// "mf on aarch64"): this slice boots to the console, the frame allocator and heap, the GIC and the timer tick.
//
// Memory is identity-mapped (`vm::direct` adds 0): the kernel image, RAM and, on demand, device registers all sit at their
// physical addresses in TTBR0, as in the embedded kernel, so the CPU-specific files of that kernel are used as they are
// (`#[path]`, below) and the two cannot drift. User address spaces will live above 512 GiB in the same TTBR0 tables.
extern crate alloc;

#[macro_use]
mod serial;
mod boot;
mod heap;
mod log;
mod pmm;
mod sync;
// the shared aarch64 files name the macro by path
pub(crate) use serial::println;

// The embedded kernel's aarch64 files, shared. They name `crate::cpu`, `crate::gic`, `crate::paging`, `crate::timer`,
// `crate::crash` and `crate::memory`, which are provided here.
#[path = "../../kernel/src/arch/aarch64/acpi.rs"]
mod acpi;
#[path = "../../kernel/src/arch/aarch64/early.rs"]
mod early;
#[path = "../../kernel/src/arch/aarch64/exceptions.rs"]
mod exceptions;
#[path = "../../kernel/src/arch/aarch64/gic.rs"]
mod gic;
#[path = "../../kernel/src/arch/aarch64/paging.rs"]
mod paging;
#[path = "../../kernel/src/arch/aarch64/pl011.rs"]
mod pl011;
#[path = "../../kernel/src/arch/aarch64/timer.rs"]
mod timer;
#[path = "../../kernel/src/arch/aarch64/cpu.rs"]
mod cpu_arch;

pub mod cpu {
    pub use crate::cpu_arch::*;

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

use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, TYPE_CONVENTIONAL};

/// Read by KUP, `fosadm` and the tests from the ELF section table, so the two kernels are never mixed up.
#[used]
#[unsafe(link_section = ".fos_flavour")]
static FLAVOUR: [u8; 2] = *b"mf";

// Entered from the UEFI loader like a C function: x0 = the BootInfo pointer, MMU and caches on with the firmware's identity
// map. The loader zeroed the BSS and applied the relocations; the image is position independent.
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

fn mib(bytes: u64) -> u64 {
    bytes >> 20
}

const PI4_GIC_DISTRIBUTOR: u64 = 0xff84_1000;
const PI4_PL011: u64 = 0xfe20_1000;

#[unsafe(no_mangle)]
pub extern "C" fn kmain(info: *const BootInfo) -> ! {
    let info = unsafe { &*info };
    exceptions::init();
    if info.is_valid() {
        // The console is where the firmware's tables say (QEMU `virt` and the Pi 4 differ), set before the first line.
        if let Some(base) = acpi::spcr_uart(info.acpi_rsdp) {
            pl011::set_base(base);
        } else if acpi::madt_gic(info.acpi_rsdp).is_some_and(|(distributor, _)| distributor == PI4_GIC_DISTRIBUTOR) {
            pl011::set_base(PI4_PL011);
        }
    }
    acpi::remember(info.acpi_rsdp);
    serial::init();
    println!();
    println!("Fosfor OS server kernel (mf, aarch64): running from the UEFI loader at EL{}.", exceptions::current_el());
    if !info.is_valid() {
        println!("BootInfo is not valid (magic {:#x}, version {}): stopping.", info.magic, info.version);
        halt();
    }
    println!("BootInfo: version {}, kernel at {:#x} ({} KiB), booted from slot {}.", info.version, info.kernel_base, info.kernel_size / 1024, info.boot_slot);
    println!("ACPI RSDP at {:#x}.", info.acpi_rsdp);
    if let Some((distributor, cpu_interface)) = acpi::madt_gic(info.acpi_rsdp) {
        gic::set_base(distributor, cpu_interface);
    }

    let Some(machine) = boot::Machine::from_info(info) else {
        println!("The memory map has more than 1024 descriptors: stopping.");
        halt();
    };
    boot::remember(&machine);
    let (mut regions, mut ram_pages, mut free_pages) = (0u64, 0u64, 0u64);
    for d in machine.descriptors() {
        regions += 1;
        if fosfor_abi::RAM_LIKE_TYPES.contains(&d.kind) {
            ram_pages += d.pages;
        }
        if d.kind == TYPE_CONVENTIONAL {
            free_pages += d.pages;
        }
    }
    println!("Memory map: {regions} regions, {} MiB of RAM-like memory, {} MiB conventional.", mib(ram_pages * 4096), mib(free_pages * 4096));
    pmm::init(&machine, &[(machine.kernel_base, machine.kernel_size), (machine.info_page, 4096), machine.map, (info.acpi_rsdp & !0xfff, 16384)]);
    let frames = pmm::stats();
    println!("Physical memory: {} MiB managed in {} frames, {} MiB free.", mib(frames.managed * 4096), frames.managed, mib(frames.free * 4096));

    paging::build_and_load(info, &info.framebuffer);
    paging::map_device(pl011::base(), 4096);
    println!("Page tables: loaded; RAM no-execute in 2 MiB blocks, kernel image W^X, page 0 and the stack guard unmapped.");

    if !heap::init() {
        println!("The kernel heap could not be created: stopping.");
        halt();
    }
    heap_self_test();
    let heap = heap::stats();
    println!("Heap: self-test passed, {} KiB in the heap, {} KiB free.", heap.total / 1024, heap.free / 1024);

    gic::init();
    cpu::interrupts_enable();
    let start = timer::micros();
    while timer::micros() - start < 100_000 {
        cpu::sleep();
    }
    println!("GIC at {:#x}/{:#x}: {} timer interrupts (INTID 30) in 100 ms, expected about 10; counter {} Hz.", gic::bases().0, gic::bases().1, gic::irq_ticks(), timer::frequency());
    println!("kernel-mf (aarch64): slice 1 reached the end of boot; parking.");
    loop {
        cpu::sleep();
    }
}

/// Boxes, growing vectors, strings, a map, an over-aligned block and growth beyond the first chunk; then checks that freeing
/// everything returns the memory.
fn heap_self_test() {
    use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
    let before = heap::stats().free;
    {
        let boxed = Box::new(0x1234_5678_u64);
        assert_eq!(*boxed, 0x1234_5678);
        let mut numbers: Vec<u32> = Vec::new();
        for n in 0..100_000 {
            numbers.push(n);
        }
        assert_eq!(numbers.iter().map(|&n| n as u64).sum::<u64>(), 100_000 * 99_999 / 2);
        let mut text = String::new();
        for n in 0..200 {
            text.push_str(&alloc::format!("{n},"));
        }
        assert!(text.starts_with("0,1,2,") && text.ends_with("199,"));
        let mut map = BTreeMap::new();
        for n in 0..1000u32 {
            map.insert(n.wrapping_mul(2_654_435_761), n);
        }
        assert_eq!(map.get(&5u32.wrapping_mul(2_654_435_761)), Some(&5));
        let aligned = alloc::alloc::Layout::from_size_align(100, 4096).unwrap();
        let block = unsafe { alloc::alloc::alloc(aligned) };
        assert!(!block.is_null() && block as usize % 4096 == 0);
        unsafe { alloc::alloc::dealloc(block, aligned) };
        let big = alloc::vec![0xa5u8; 6 << 20];
        assert!(big.iter().all(|&b| b == 0xa5));
        assert!(heap::stats().total > 2 << 20);
    }
    let after = heap::stats().free;
    assert!(after >= before, "the heap lost memory: {before} free before, {after} after");
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

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    serial::break_lock();
    println!("\nFosfor OS server kernel PANIC: {info}");
    halt()
}
