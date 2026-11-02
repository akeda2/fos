//! The server kernel ("FOS-mf"). It is entered by the same UEFI and BIOS loaders as the embedded kernel, with the same
//! `BootInfo`; it narrates over COM1. Slice 2 adds memory: the frame allocator, the kernel's own page tables (direct map,
//! W^X image) and the heap. SMP and the consoles follow in the slices of docs/big-kernel.md ("Stage 2 design").
#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod serial;
mod acpi;
mod apic;
mod boot;
mod cpu;
#[path = "../../kernel/src/arch/x86_64/gdt.rs"]
mod gdt;
mod heap;
#[path = "../../kernel/src/arch/x86_64/idt.rs"]
mod idt;
mod pmm;
mod trap;
mod vm;

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, TYPE_CONVENTIONAL};

core::arch::global_asm!(include_str!("../../kernel/src/arch/x86_64/isr_stubs.S"));

unsafe extern "C" {
    static __ist_top: u8;
}

/// Read by KUP, `fosadm` and the tests from the ELF section table, so the two kernels are never mixed up.
#[used]
#[unsafe(link_section = ".fos_flavour")]
static FLAVOUR: [u8; 2] = *b"mf";

core::arch::global_asm!(
    r#"
    .section .text.entry, "ax"
    .global _start
    _start:
        cli                              # the firmware's interrupt state is not ours
        cld
        lea rsp, [rip + __stack_top]     # our own stack, at the end of .bss
        xor ebp, ebp
        call kmain                       # rdi still holds the BootInfo pointer
    2:  hlt
        jmp 2b
    "#
);

pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

fn mib(bytes: u64) -> u64 {
    bytes >> 20
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn kmain(info: *const BootInfo) -> ! {
    serial::init();
    println!();
    println!("Fosfor OS server kernel (mf, slice 3): running in 64-bit long mode.");

    let info = unsafe { &*info };
    if !info.is_valid() {
        println!("BootInfo is not valid (magic {:#x}, version {}): stopping.", info.magic, info.version);
        halt();
    }
    println!("BootInfo: version {}, kernel at {:#x} ({} KiB), booted from slot {}.", info.version, info.kernel_base, info.kernel_size / 1024, info.boot_slot);
    let fb = &info.framebuffer;
    println!("Framebuffer: {}x{} at {:#x}. ACPI RSDP at {:#x}.", fb.width, fb.height, fb.base, info.acpi_rsdp);

    // Everything the kernel needs from loader memory is copied out before the switch to our own page tables.
    let Some(machine) = boot::Machine::from_info(info) else {
        println!("The memory map has more than 1024 descriptors: stopping.");
        halt();
    };

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

    pmm::init(&machine, &[(machine.kernel_base, machine.kernel_size), (machine.info_page, 4096), machine.map]);
    let frames = pmm::stats();
    println!("Physical memory: {} MiB managed in {} frames, {} MiB free.", mib(frames.managed * 4096), frames.managed, mib(frames.free * 4096));

    let mapped = vm::init(&machine);
    println!("Page tables: root {:#x}; direct map of {} MiB at {:#x}, no-execute; the kernel image keeps its own mapping.", vm::root(), mib(mapped), vm::DIRECT_BASE);
    if !vm::check_image() {
        println!("Page tables: the kernel image does not have the W^X layout: stopping.");
        halt();
    }
    println!("Page tables: W^X holds (text R-X, rodata R--, data RW-, stack guards and page 0 unmapped).");

    // A frame written and read back through the direct map, and the tables agree where it is.
    let frame = pmm::allocate(1, None).expect("a frame");
    let through_direct = vm::direct(frame) as *mut u64;
    unsafe { through_direct.write_volatile(0xf05f_05f0_f05f_05f0) };
    assert_eq!(unsafe { through_direct.read_volatile() }, 0xf05f_05f0_f05f_05f0);
    assert_eq!(vm::translate(vm::direct(frame)).map(|m| m.physical), Some(frame));
    pmm::free(frame, 1);
    println!("Direct map: frame {frame:#x} written and read back at {:#x}.", vm::direct(frame));

    if !heap::init() {
        println!("The kernel heap could not be created: stopping.");
        halt();
    }
    heap_self_test();
    let heap = heap::stats();
    println!("Heap: self-test passed, {} KiB in the heap, {} KiB free.", heap.total / 1024, heap.free / 1024);

    // ---- interrupts ----
    gdt::load(&raw const __ist_top as u64);
    idt::load();
    let before = trap::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed);
    unsafe { core::arch::asm!("int3") };
    assert_eq!(trap::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed), before + 1);
    println!("Interrupts: GDT, TSS (double-fault stack) and IDT loaded; a breakpoint came back through the IDT.");

    // ---- ACPI, APICs, timer ----
    let Some(tables) = acpi::open(machine.acpi_rsdp) else {
        println!("ACPI: no valid RSDP: stopping.");
        halt();
    };
    let (Some(fadt), Some(madt)) = (acpi::fadt(&tables), acpi::madt(&tables)) else {
        println!("ACPI: FADT or MADT missing: stopping.");
        halt();
    };
    println!("ACPI: revision {}, local APIC at {:#x}, {} CPU(s), {} I/O APIC(s), PM timer port {:#x}.", tables.revision, madt.local_apic_address, madt.cpu_count, madt.io_apics, fadt.pm_timer_port);
    match acpi::srat(&tables) {
        Some(srat) => println!("SRAT: {} NUMA node(s), {} memory range(s), {} CPU affinity entries.", srat.nodes, srat.memory_count, srat.cpu_affinities),
        None => println!("SRAT: none (one NUMA node)."),
    }
    let Some(timer) = apic::start_timer(&fadt, madt.local_apic_address) else {
        println!("APIC: no local APIC or PM timer: stopping.");
        halt();
    };
    println!("Local APIC: id {}, {} at {:#x}, timer {} kHz.", apic::id(), if timer.x2apic { "x2APIC" } else { "xAPIC" }, timer.base, timer.timer_hz / 1000);
    match apic::ioapic::init(&madt) {
        Some(pins) => println!("I/O APIC: {pins} pins, all masked."),
        None => println!("I/O APIC: none."),
    }
    cpu::interrupts_enable();
    let start = apic::ticks();
    apic::busy_wait_ms(200);
    let ticks = apic::ticks() - start;
    println!("APIC timer: {} Hz tick, {ticks} ticks in 200 ms of PM-timer time.", apic::TICK_HZ);
    cpu::interrupts_disable();
    println!("kernel-mf: slice 3 reached the end of boot; halting.");
    halt();
}

/// Exercises the heap the way the kernel will: boxes, growing vectors, strings, a map, an over-aligned block, and growth
/// beyond the first chunk; then checks that freeing everything returns the memory.
fn heap_self_test() {
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
        assert_eq!(map.len(), 1000);
        assert_eq!(map.get(&5u32.wrapping_mul(2_654_435_761)), Some(&5));

        let aligned = alloc::alloc::Layout::from_size_align(100, 4096).unwrap();
        let block = unsafe { alloc::alloc::alloc(aligned) };
        assert!(!block.is_null() && block as usize % 4096 == 0);
        unsafe { alloc::alloc::dealloc(block, aligned) };

        // More than the first chunk: the heap has to grow.
        let big = alloc::vec![0xa5u8; 6 << 20];
        assert!(big.iter().all(|&b| b == 0xa5));
        assert!(heap::stats().total > 2 << 20);
    }
    let after = heap::stats().free;
    assert!(after >= before, "the heap lost memory: {before} free before, {after} after");
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nFosfor OS server kernel PANIC: {info}");
    halt()
}
