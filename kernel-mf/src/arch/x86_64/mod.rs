//! Everything the server kernel does differently on x86-64: the entry, GDT/IDT/TSS, ACPI, the APICs, SMP start, TLB
//! shootdowns, the page-table hardware access and the `fos_hal::Platform`. The shared kernel (`main.rs`, `sched`, `shell`,
//! the drivers) reaches it through the root re-exports and the hooks below; the aarch64 module offers the same hooks.
//! Modules keep their short crate-root names (`crate::apic`, `crate::cpu`, ...) through `pub use arch::*`.

pub mod acpi;
pub mod apic;
pub mod context;
pub mod cpu;
pub mod cpuinfo;
pub mod crashrec;
pub mod hwwd;
pub mod power;
pub mod fpu;
pub mod gdt;
pub mod idt;
pub mod mmu;
mod percpu_arch;
pub mod pci_config;
pub mod platform;
pub mod smp;
pub mod tlb;
pub mod trap;
pub mod usermode;
pub mod user_blobs;

pub use apic::ioapic;
pub use context::{WAKE_VECTOR, end_wake_interrupt, prepare_stack, send_wake_ipi, switch};
pub use percpu_arch::{CpuArch, activate, cpu_pointer, interrupt_target, set_kernel_stack};

use crate::{boot, percpu, pmm, process, vm};
use fosfor_abi::BootInfo;

// The embedded kernel's interrupt-routing file, included as it is so the two kernels cannot drift. It names `crate::apic`,
// `crate::ioapic` and `crate::pci`, which are provided here and at the root.
#[path = "../../../../kernel/src/arch/x86_64/msi.rs"]
pub mod msi;
// The embedded kernel's wall clock (CMOS RTC read once, then advanced by the microsecond count), shared so the two cannot drift.
pub mod timer {
    pub fn micros() -> u64 {
        crate::apic::micros()
    }
}
#[path = "../../../../kernel/src/clock.rs"]
pub mod clock;

/// The embedded kernel's `install` and `bootcode` use physical memory as pointers (identity map); here a buffer's
/// "address" is its direct-map address and `free` takes it back to the physical frame.
pub mod memory {
    pub const FRAME: u64 = crate::pmm::FRAME;
    pub fn allocate_contiguous(count: u64, below: Option<u64>) -> Option<u64> {
        crate::pmm::allocate(count, below).map(crate::vm::direct)
    }
    pub fn free(address: u64, count: u64) {
        crate::pmm::free(address - crate::vm::direct(0), count);
    }
}

core::arch::global_asm!(include_str!("isr_stubs.S"));

unsafe extern "C" {
    static __ist_top: u8;
}

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

/// Second half of the banner line.
pub fn running_on() -> &'static str {
    "running in 64-bit long mode."
}

/// Memory the loader leaves in use beyond the kernel, its pages and the memory map (none here: ACPI is read through the
/// direct map from where the firmware left it).
pub fn reserved_extra(_info: &BootInfo) -> (u64, u64) {
    (0, 0)
}

/// May the kernel read the 8 bytes at `address` (a backtrace's stack words)? Walks the kernel's tables, takes no lock.
pub fn readable(address: u64) -> bool {
    address & 7 == 0 && vm::translate(address).is_some()
}

/// The caller's frame pointer (rbp; kernel-mf keeps frame pointers).
#[inline(always)]
pub fn frame_pointer() -> u64 {
    let fp: u64;
    unsafe { core::arch::asm!("mov {}, rbp", out(reg) fp, options(nomem, nostack, preserves_flags)) };
    fp
}

/// Sends a non-maskable interrupt to every CPU in `mask` (bit per index): `debugcpu::capture_all`. An NMI also stops a CPU that
/// spins with interrupts off.
pub fn capture_cpus(mask: u128) {
    for cpu in (0..percpu::count()).filter(|&cpu| mask & 1 << cpu != 0) {
        apic::send_ipi(percpu::get(cpu).arch.apic_id, 0x4400); // delivery mode NMI (0b100), assert
    }
}

pub fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

/// `debug idle poll`: idle CPUs spin instead of `hlt`, so none enters a sleep state the firmware may deepen (C1E, package
/// C-states) and an interrupt is taken at once. An experiment for audio interrupt latency; it costs power and heat.
static IDLE_POLL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn set_idle_poll(on: bool) -> bool {
    IDLE_POLL.store(on, core::sync::atomic::Ordering::Relaxed);
    true
}

pub fn idle_poll() -> bool {
    IDLE_POLL.load(core::sync::atomic::Ordering::Relaxed)
}

/// The Intel PCH's `SMI_EN` and `SMI_STS` (I/O `PMBASE + 0x30`, `+ 0x34`), read only: which firmware sources may raise an
/// SMI and which have (status bits are sticky until the firmware clears them). PMBASE is the PM1a event block, 8 below the PM
/// timer on that chipset. `None` without an Intel CPU or a PM timer.
pub fn smi_sources() -> Option<(u32, u32)> {
    smi_count()?;
    let timer = apic::pm_port();
    if timer < 8 {
        return None;
    }
    let base = timer - 8;
    Some((cpu::inl(base + 0x30), cpu::inl(base + 0x34)))
}

/// `SMI_EN` as the firmware left it, saved at the first `debug smi on|off` so `debug smi restore` can put it back.
static SMI_EN_FIRMWARE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(u64::MAX);

/// `debug smi on|off BIT`: sets or clears one `SMI_EN` source and returns the register before and after (read back: a
/// bit the firmware locked stays as it was). An experiment, not a setting: the firmware owns this register, may set the
/// bit again in its next SMI, and may rely on the source (bit 6, the software SMI timer, re-arms itself every 1.5 to 64 ms
/// and is often how firmware polls legacy USB). Bit 0 (all SMIs) is refused by the caller. `None` where `smi_sources` is.
pub fn smi_enable_bit(bit: u32, on: bool) -> Option<(u32, u32)> {
    let (before, _) = smi_sources()?;
    let _ = SMI_EN_FIRMWARE.compare_exchange(u64::MAX, before as u64, core::sync::atomic::Ordering::Relaxed, core::sync::atomic::Ordering::Relaxed);
    let value = if on { before | 1 << bit } else { before & !(1 << bit) };
    let port = apic::pm_port() - 8 + 0x30;
    cpu::outl(port, value);
    Some((before, cpu::inl(port)))
}

/// Writes back the `SMI_EN` saved before the first change; `None` if nothing was changed.
pub fn smi_restore() -> Option<(u32, u32)> {
    let saved = SMI_EN_FIRMWARE.load(core::sync::atomic::Ordering::Relaxed);
    if saved == u64::MAX {
        return None;
    }
    let (before, _) = smi_sources()?;
    let port = apic::pm_port() - 8 + 0x30;
    cpu::outl(port, saved as u32);
    Some((before, cpu::inl(port)))
}

/// System management interrupts since reset (Intel `MSR_SMI_COUNT`, 0x34, family 6 from Nehalem, model 0x1a, on). An SMI
/// runs firmware with every CPU stopped, invisible to the kernel but not to an audio card's deadline. `None` on other CPUs,
/// where reading the MSR would fault.
pub fn smi_count() -> Option<u64> {
    let (_, ebx, ecx, edx) = cpu::cpuid(0, 0);
    let intel = (ebx, edx, ecx) == (u32::from_le_bytes(*b"Genu"), u32::from_le_bytes(*b"ineI"), u32::from_le_bytes(*b"ntel"));
    let signature = cpu::cpuid(1, 0).0;
    let family = (signature >> 8) & 0xf;
    let model = ((signature >> 4) & 0xf) | ((signature >> 12) & 0xf0);
    (intel && family == 6 && model >= 0x1a).then(|| cpu::read_msr(0x34) & 0xffff_ffff)
}

/// Sleeps until the next interrupt (interrupts are enabled for the wait); with `debug idle poll`, spins about 10 µs with
/// interrupts on and returns, and the idle loop looks at the run queue again.
pub fn wait_for_interrupt() {
    if idle_poll() {
        unsafe { core::arch::asm!("sti", options(nomem, nostack)) };
        for _ in 0..1000 {
            core::hint::spin_loop();
        }
        return;
    }
    unsafe { core::arch::asm!("sti", "hlt", options(nomem, nostack)) };
}

pub use apic::TICK_HZ;

/// Scheduler ticks since the timer started.
pub fn ticks() -> u64 {
    apic::ticks()
}

pub fn micros() -> u64 {
    apic::micros()
}

/// A free-running counter, only good for mixing into seeds and timing short spans (the TSC).
pub fn cycle_counter() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

pub fn busy_wait_ms(ms: u64) {
    apic::busy_wait_ms(ms)
}

/// Before anything is printed: nothing to do on x86 (the COM1 UART needs no table lookups).
pub fn early(_info: &BootInfo) {}

/// Our own page tables (direct map, W^X image) and the console that needs them. Returns the BootInfo as seen through the
/// direct map: the loader's page is not reachable at its own address afterwards.
pub fn init_memory(machine: &boot::Machine, _info: &BootInfo) -> &'static BootInfo {
    let mapped = vm::init(machine);
    let info = unsafe { &*(vm::direct(machine.info_page) as *const BootInfo) };
    println!("Page tables: root {:#x}; direct map of {} MiB at {:#x}, no-execute; the kernel image keeps its own mapping.", vm::root(), crate::mib(mapped), vm::DIRECT_BASE);
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
    match crate::fb::init(&info.framebuffer) {
        Some((columns, rows, combining)) => {
            println!("Framebuffer console: {}x{} pixels, {columns}x{rows} characters (also on COM1 and the network).", info.framebuffer.width, info.framebuffer.height);
            println!("Framebuffer: {}.", if combining { "write-combining (PAT)" } else { "uncached (no PAT, or a mapping that cannot change)" });
        }
        None if info.runtime_services == 0 => {
            let (columns, rows) = crate::fb::init_text();
            println!("Framebuffer console: none; using the BIOS's {columns}x{rows} VGA text screen (also on COM1 and the network).");
        }
        None => println!("Framebuffer console: no pixel screen (serial and network only)."),
    }
    println!("Direct map: frame {frame:#x} written and read back at {:#x}.", vm::direct(frame));
    println!("Lower half: {} (drivers reach RAM and device registers through the direct map).", if alias_gone { "empty apart from the kernel image, no identity alias" } else { "STILL ALIASED" });
    info
}

/// Per-CPU state, the IDT, and a breakpoint that must come back through it.
pub fn init_interrupts() {
    activate(0, &raw const __ist_top as u64);
    idt::build();
    idt::load();
    usermode::init_cpu();
    crate::fpu::init_cpu();
    let before = trap::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed);
    unsafe { core::arch::asm!("int3") };
    assert_eq!(trap::BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed), before + 1);
    println!("Interrupts: per-CPU GDT and TSS (double-fault stack), the IDT and the GS base are set; a breakpoint came back through the IDT.");
}

/// ACPI, the APICs, the timer and the other CPUs; interrupts are on when it returns. Returns the century register for the clock.
pub fn init_machine(machine: &boot::Machine) -> u8 {
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
    power::set_acpi_s5(fadt.pm1a_control, fadt.pm1b_control, fadt.s5);
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
                println!("NUMA node {node}: {} MiB of memory, {} MiB free.", crate::mib(total), crate::mib(free));
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
    let smp = smp::start_all(machine, &madt, srat.as_ref());
    println!("SMP: {} CPU(s) online ({} application processor(s) started, {} failed).", 1 + smp.started, smp.started, smp.failed);
    for index in 0..percpu::count() {
        let cpu = percpu::get(index);
        println!("CPU {index}: APIC id {}, NUMA node {}, {}.", cpu.arch.apic_id, cpu.node, if index == 0 { "boot processor" } else { "application processor" });
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
    fadt.century
}

/// `debug crash-on-boot`: a deliberate fault.
pub fn crash_now() {
    unsafe { core::arch::asm!("ud2") };
}
