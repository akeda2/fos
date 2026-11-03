//! The server kernel ("FOS-mf"). It is entered by the same UEFI and BIOS loaders as the embedded kernel, with the same
//! `BootInfo`; it narrates over COM1. Slice 2 adds memory: the frame allocator, the kernel's own page tables (direct map,
//! W^X image) and the heap. SMP and the consoles follow in the slices of docs/big-kernel.md ("Stage 2 design").
#![no_std]
#![no_main]

extern crate alloc;

#[macro_use]
mod serial;
mod fpu;
mod process;
mod sched;
mod tapeapi;
mod tapeaudio;
mod tapefs;
mod tapenet;
mod audioshell;
mod usbshell;
mod ps2;
mod usb;
mod user_test;
mod fb;
mod config;
mod debug;
mod hwwd;
mod net;
mod netfs;
mod netclient;
mod tapesync;
mod power;
mod shell;
mod shellhost;
mod sysshell;
mod log;
#[path = "../../kernel/src/formatshell.rs"]
mod formatshell;
/// The embedded kernel's `install` and `bootcode` use physical memory as pointers (identity map); here a buffer's
/// "address" is its direct-map address and `free` takes it back to the physical frame.
mod memory {
    pub const FRAME: u64 = crate::pmm::FRAME;
    pub fn allocate_contiguous(count: u64, below: Option<u64>) -> Option<u64> {
        crate::pmm::allocate(count, below).map(crate::vm::direct)
    }
    pub fn free(address: u64, count: u64) {
        crate::pmm::free(address - crate::vm::direct(0), count);
    }
}
#[path = "../../kernel/src/install.rs"]
mod install;
#[path = "../../kernel/src/bootcode.rs"]
mod bootcode;
use kernel_core::{fatvol, fosfs, gpt};
// formatshell (shared with the embedded kernel) names the macro by path
pub(crate) use serial::println;
mod slots;
mod acpi;
mod apic;
mod boot;
mod cpu;
mod gdt;
mod heap;
mod idt;
mod block;
mod percpu;
mod pci_config;
mod platform;
mod pmm;
mod smp;
mod storage;
mod sync;
mod tlb;
mod trap;
mod vm;

// The embedded kernel's interrupt-routing file, included as it is so the two kernels cannot drift. It names `crate::apic`,
// `crate::ioapic` and `crate::pci`, which are provided here.
#[path = "../../kernel/src/arch/x86_64/msi.rs"]
mod msi;
// The embedded kernel's wall clock (CMOS RTC read once, then advanced by the microsecond count), shared so the two cannot drift.
mod timer {
    pub fn micros() -> u64 {
        crate::apic::micros()
    }
}
#[path = "../../kernel/src/clock.rs"]
mod clock;
use apic::ioapic;
use kernel_core::pci;

use alloc::{boxed::Box, collections::BTreeMap, string::String, vec::Vec};
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, TYPE_CONVENTIONAL};

core::arch::global_asm!(include_str!("isr_stubs.S"));

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
    println!("Fosfor OS server kernel (mf, slice 8): running in 64-bit long mode.");

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
    // The loader's page is no longer reachable at its own address: use it through the direct map from here on.
    let info = unsafe { &*(vm::direct(machine.info_page) as *const BootInfo) };
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
    // No identity alias any more: the frame's own address (outside the kernel image) is not mapped in the lower half.
    let alias_gone = vm::translate(frame).is_none();
    pmm::free(frame, 1);
    match fb::init(&info.framebuffer) {
        Some((columns, rows, combining)) => {
            println!("Framebuffer console: {}x{} pixels, {columns}x{rows} characters (also on COM1 and the network).", info.framebuffer.width, info.framebuffer.height);
            println!("Framebuffer: {}.", if combining { "write-combining (PAT)" } else { "uncached (no PAT, or a mapping that cannot change)" });
        }
        None if info.runtime_services == 0 => {
            let (columns, rows) = fb::init_text();
            println!("Framebuffer console: none; using the BIOS's {columns}x{rows} VGA text screen (also on COM1 and the network).");
        }
        None => println!("Framebuffer console: no pixel screen (serial and network only)."),
    }
    println!("Direct map: frame {frame:#x} written and read back at {:#x}.", vm::direct(frame));
    println!("Lower half: {} (drivers reach RAM and device registers through the direct map).", if alias_gone { "empty apart from the kernel image, no identity alias" } else { "STILL ALIASED" });

    if !heap::init() {
        println!("The kernel heap could not be created: stopping.");
        halt();
    }
    heap_self_test();
    let heap = heap::stats();
    println!("Heap: self-test passed, {} KiB in the heap, {} KiB free.", heap.total / 1024, heap.free / 1024);

    // ---- interrupts ----
    percpu::activate(0, &raw const __ist_top as u64);
    idt::build();
    idt::load();
    process::init_cpu();
    fpu::init_cpu();
    let before = trap::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed);
    unsafe { core::arch::asm!("int3") };
    assert_eq!(trap::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed), before + 1);
    println!("Interrupts: per-CPU GDT and TSS (double-fault stack), the IDT and the GS base are set; a breakpoint came back through the IDT.");

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
    power::set_acpi_reset(fadt.reset);
    let srat = acpi::srat(&tables);
    match &srat {
        Some(srat) => {
            println!("SRAT: {} NUMA node(s), {} memory range(s), {} CPU affinity entries.", srat.nodes, srat.memory_count, srat.cpu_affinities);
            for node in 0..srat.nodes as u32 {
                let (mut total, mut free) = (0, 0);
                for &(start, length, owner) in &srat.memory[..srat.memory_count] {
                    if owner == node {
                        total += length;
                        free += pmm::free_in(start, length) * pmm::FRAME;
                    }
                }
                println!("NUMA node {node}: {} MiB of memory, {} MiB free.", mib(total), mib(free));
            }
        }
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
    let smp = smp::start_all(&machine, &madt, srat.as_ref());
    println!("SMP: {} CPU(s) online ({} application processor(s) started, {} failed).", 1 + smp.started, smp.started, smp.failed);
    for index in 0..percpu::count() {
        let cpu = percpu::get(index);
        println!("CPU {index}: APIC id {}, NUMA node {}, {}.", cpu.apic_id, cpu.node, if index == 0 { "boot processor" } else { "application processor" });
    }
    cpu::interrupts_enable();
    let start = apic::ticks();
    apic::busy_wait_ms(200);
    let ticks = apic::ticks() - start;
    println!("APIC timer: {} Hz tick, {ticks} ticks in 200 ms of PM-timer time.", apic::TICK_HZ);
    for index in 1..percpu::count() {
        println!("CPU {index}: {} timer ticks since it came up.", percpu::ticks_of(index));
    }
    println!("Clock: {} (microseconds {}).", apic::micros_source(), apic::micros());
    clock::init(fadt.century);

    // Device drivers: `kernel-core` reaches the machine through the platform below. Interrupts stay on, as the drivers wait on ticks.
    platform::install();
    let devices = pci::scan();
    println!("PCI: {devices} function(s) found.");
    for device in pci::devices() {
        println!("  {:02x}:{:02x}.{} {:04x}:{:04x} {}", device.bus, device.device, device.function, device.vendor, device.id, pci::class_name(device.class, device.subclass));
    }
    storage::init(info);
    // A USB stick is a disk only once USB is up; a machine that boots from one gets USB before its config is read (so `no-msi`
    // in `settings` cannot apply to USB on such a machine).
    let usb_first = storage::boot_disk().is_none();
    if usb_first {
        usb::init();
        storage::find_boot_disk(info);
    }
    if let Some(disk) = storage::boot_disk() {
        slots::init(info, disk);
    }
    debug::init(info.crash_count);
    debug::report_previous_crash();
    config::load();
    kernel_core::hda::init_all(config::no_msi());
    if config::crash_on_boot() {
        println!("debug: crash-on-boot is set; faulting now.");
        unsafe { core::arch::asm!("ud2") };
    }
    slots::confirm_boot();
    net::init();
    if !usb_first {
        usb::init();
    }
    ps2::init();
    sched_selftest();
    sched::spawn("service", service_thread, 0).expect("the service thread");
    println!("kernel-mf: slice 8 reached the end of boot; entering the service loop.");
    // The boot context is the idle thread: it runs when nothing else is ready and sleeps until the next interrupt.
    loop {
        sched::yield_now();
        // Interrupts are on: the next tick (or NIC interrupt) wakes the CPU.
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    }
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
    serial::break_lock();
    println!("\nFosfor OS server kernel PANIC: {info}");
    halt()
}

/// The server's housekeeping: NICs, serial input, the reset timer and the slot streak. Polls once per pass and gives way.
extern "C" fn service_thread(_: usize) {
    // The `start` file runs here, as at the end of the embedded kernel's boot; the prompt follows it.
    config::run_start(shell::run);
    shell::prompt();
    loop {
        net::poll();
        usb::poll();
        ps2::poll();
        while let Some(byte) = serial::read_byte() {
            shell::feed(byte);
        }
        net::housekeeping();
        slots::housekeeping();
        debug::pet();
        sched::yield_now();
    }
}

static SELFTEST_STEPS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

extern "C" fn selftest_worker(steps: usize) {
    for _ in 0..steps {
        SELFTEST_STEPS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        sched::yield_now();
    }
}

/// Starts three threads that each take five steps, interleaved by yielding, and checks that they all finish and that their
/// stacks (frames and mappings) come back.
static SPIN_STOP: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static SPIN_COUNT: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];
static SHARED: sched::SleepMutex<u64> = sched::SleepMutex::new(0);
static SLEPT_TICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static FINISHED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Never yields: only the timer tick can take the CPU from it.
extern "C" fn spinner(index: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    while !SPIN_STOP.load(Relaxed) {
        SPIN_COUNT[index].fetch_add(1, Relaxed);
    }
    FINISHED.fetch_add(1, Relaxed);
}

/// Read-modify-write under the sleeping mutex with a yield in the middle, so the other thread contends and has to block.
extern "C" fn incrementer(rounds: usize) {
    for _ in 0..rounds {
        let mut guard = SHARED.lock();
        let value = *guard;
        sched::yield_now();
        *guard = value + 1;
    }
    FINISHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

extern "C" fn sleeper(ticks: usize) {
    let start = apic::ticks();
    sched::sleep_ticks(ticks as u64);
    SLEPT_TICKS.store(apic::ticks() - start, core::sync::atomic::Ordering::Relaxed);
    FINISHED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// Preemption, sleeping and the sleeping mutex: two threads that never yield both make progress, a sleeper wakes on time and
/// two contending incrementers lose no update.
fn sched_preempt_selftest() {
    use core::sync::atomic::Ordering::Relaxed;
    for index in 0..2 {
        sched::spawn("spinner", spinner, index).expect("a spinner");
    }
    for _ in 0..2 {
        sched::spawn("incrementer", incrementer, 25).expect("an incrementer");
    }
    sched::spawn("sleeper", sleeper, 20).expect("a sleeper");
    let start = apic::ticks();
    // The boot thread is the idle thread and may not block: it sleeps in `hlt` until the tick brings it back.
    while apic::ticks() - start < 60 {
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
    }
    SPIN_STOP.store(true, Relaxed);
    let mut waited = 0;
    while FINISHED.load(Relaxed) < 5 && waited < 300 {
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
        waited += 1;
    }
    for _ in 0..4 {
        sched::yield_now();
    }
    let (a, b) = (SPIN_COUNT[0].load(Relaxed), SPIN_COUNT[1].load(Relaxed));
    let total = *SHARED.lock();
    let slept = SLEPT_TICKS.load(Relaxed);
    println!("Scheduler: preemption {} ({} and {} spins by two threads that never yield), sleep {} ({slept} ticks for 20), mutex {} ({total} of 50 increments).",
        if a > 0 && b > 0 { "ok" } else { "FAILED" }, a, b, if (20..=23).contains(&slept) { "ok" } else { "FAILED" }, if total == 50 { "ok" } else { "FAILED" });
}

static SMP_SEEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static SMP_DONE: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

extern "C" fn smp_worker(_: usize) {
    use core::sync::atomic::Ordering::Relaxed;
    let until = apic::ticks() + 5;
    while apic::ticks() < until {
        SMP_SEEN.fetch_or(1 << percpu::this().index, Relaxed);
        core::hint::spin_loop();
    }
    SMP_DONE.fetch_add(1, Relaxed);
}

/// Unpinned threads that spin for a while are picked up by every online CPU (an idle application processor looks at each tick).
fn sched_smp_selftest() {
    use core::sync::atomic::Ordering::Relaxed;
    let cpus = percpu::count();
    let workers = 2 * cpus;
    for _ in 0..workers {
        sched::spawn_any("smp-worker", smp_worker, 0).expect("an smp worker");
    }
    let start = apic::ticks();
    while SMP_DONE.load(Relaxed) < workers && apic::ticks() - start < 300 {
        unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
        sched::yield_now();
    }
    for _ in 0..4 {
        sched::yield_now();
    }
    let seen = SMP_SEEN.load(Relaxed).count_ones() as usize;
    println!("Scheduler: SMP {} ({} unpinned threads ran on {seen} of {cpus} CPU(s)).", if seen == cpus && SMP_DONE.load(Relaxed) == workers { "ok" } else { "FAILED" }, workers);
}

static WAKE_STAMP: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

extern "C" fn wake_worker(_: usize) {
    WAKE_STAMP.store(apic::micros(), core::sync::atomic::Ordering::Release);
}

/// A thread spawned while the other CPUs idle starts within the IPI's latency, not at their next 10 ms tick (which would
/// average 5 ms).
fn sched_wake_selftest() {
    use core::sync::atomic::Ordering::{Acquire, Relaxed};
    let cpus = percpu::count();
    if cpus < 2 {
        return;
    }
    for _ in 0..4 {
        sched::yield_now();
    }
    let mut total = 0;
    const TRIALS: u64 = 10;
    for _ in 0..TRIALS {
        WAKE_STAMP.store(0, Relaxed);
        apic::busy_wait_ms(3);
        let begin = apic::micros();
        sched::spawn_any("wake-worker", wake_worker, 0).expect("a wake worker");
        let mut spins = 0u64;
        while WAKE_STAMP.load(Acquire) == 0 && spins < 100_000_000 {
            core::hint::spin_loop();
            spins += 1;
        }
        total += WAKE_STAMP.load(Acquire).saturating_sub(begin);
    }
    let average = total / TRIALS;
    println!("Scheduler: wake IPI {} (a thread spawned on {cpus} idle CPU(s) started after {average} us on average).", if average < 2000 { "ok" } else { "FAILED" });
}

fn sched_selftest() {
    sched::init();
    // The first stack creates the page-table pages of the stack region, which stay: do that before counting frames.
    sched::spawn("warmup", selftest_worker, 0).expect("a warm-up thread");
    for _ in 0..4 {
        sched::yield_now();
    }
    let free_before = pmm::stats().free;
    for name in ["worker-a", "worker-b", "worker-c"] {
        sched::spawn(name, selftest_worker, 5).expect("a worker thread");
    }
    let mut passes = 0;
    while SELFTEST_STEPS.load(core::sync::atomic::Ordering::Relaxed) < 15 && passes < 1000 {
        sched::yield_now();
        passes += 1;
    }
    // One more pass each lets the last switch release the final dead thread.
    for _ in 0..4 {
        sched::yield_now();
    }
    let mut left = 0;
    sched::for_each(|id, name, _| left += (id == 0 || !name.starts_with("idle")) as usize);
    let steps = SELFTEST_STEPS.load(core::sync::atomic::Ordering::Relaxed);
    let leaked = free_before - pmm::stats().free;
    println!("Scheduler: 3 kernel threads took {steps} interleaved steps in {} context switches; {left} thread(s) left, {leaked} stack frame(s) not returned.", sched::switches());
    println!("TLB: {} shootdown(s) so far (every exited thread's stack is flushed on all {} CPU(s) before its slot is reused).", tlb::SHOOTDOWNS.load(core::sync::atomic::Ordering::Relaxed), percpu::count());
    if steps != 15 || left != 1 || leaked != 0 {
        println!("Scheduler self-test FAILED.");
    }
    sched_preempt_selftest();
    sched_smp_selftest();
    sched_wake_selftest();
    user_test::run();
}
