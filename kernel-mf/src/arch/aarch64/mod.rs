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
// PSCI on QEMU, the BCM2711 power-management watchdog on a Pi 4 (see the file's header).
#[path = "../../../../kernel/src/arch/aarch64/power.rs"]
mod power_arch;

pub mod power {
    pub use super::power_arch::{crash_reset, reset};
    pub fn shutdown() {
        super::power_arch::off()
    }
}

pub mod crashrec;
pub mod hwwd;

pub mod cpu {
    pub use super::cpu_arch::*;

    /// A lock may be waited for about 5 s (the generic counter runs at 19.2-62.5 MHz).
    pub const DEADLOCK_CYCLES: u64 = 300_000_000;

    /// The free-running counter the spin-lock deadlock detector uses.
    pub fn cycles() -> u64 {
        crate::timer::counter()
    }
}

// PCI configuration space (ECAM from the ACPI MCFG table, or the Pi 4's controller) and MSI delivery (GICv2m, or the Pi 4's block).
#[path = "../../../../kernel/src/arch/aarch64/pci_config.rs"]
pub mod pci_config;
#[path = "../../../../kernel/src/arch/aarch64/msi.rs"]
pub mod msi;
pub mod platform;

pub mod context;
pub mod cpuinfo;
pub mod mmu;
mod percpu_arch;
pub use context::{end_wake_interrupt, prepare_stack, send_wake_ipi, switch};
pub use percpu_arch::{CpuArch, activate, cpu_pointer, set_kernel_stack};

pub mod fpu;

/// The GIC's broadcast TLB invalidates reach every CPU, so a "shootdown" is just the invalidate.
pub mod tlb {
    use core::sync::atomic::AtomicU64;
    pub static SHOOTDOWNS: AtomicU64 = AtomicU64::new(0);
    pub fn shootdown(start: u64, pages: u64) {
        for page in 0..pages {
            crate::paging::invalidate_page(start + page * 4096);
        }
        SHOOTDOWNS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

// The embedded kernel's wall clock: no RTC on the Pi, so it is unset until `date set` (or a time source) gives it a base.
#[path = "../../../../kernel/src/clock.rs"]
pub mod clock;

/// What the shared aarch64 files ask of the embedded kernel's `memory` module.
pub mod memory {
    pub const FRAME: u64 = crate::pmm::FRAME;
    pub fn allocate() -> Option<u64> {
        crate::pmm::allocate(1, None)
    }
    pub fn allocate_contiguous(count: u64, below: Option<u64>) -> Option<u64> {
        crate::pmm::allocate(count, below)
    }
    pub fn free(address: u64, count: u64) {
        crate::pmm::free(address, count);
    }

    /// Sets aside `frames` below 4 GiB as the DMA pool, mapped non-cacheable, for devices that are not cache-coherent (the
    /// Pi 4's PCIe bridge). Afterwards every allocation with a `below` limit comes from it and nothing else does (see `pmm`).
    /// Call once, after paging is up.
    pub fn dma_pool(frames: u64) {
        let Some(start) = crate::pmm::allocate_uncleared(frames, Some(1 << 32)) else { return };
        crate::paging::map_uncached(start, frames * FRAME);
        crate::pmm::free(start, frames);
        crate::pmm::set_pool(start / FRAME, start / FRAME + frames);
        crate::println!("DMA pool: {} KiB at {:#x}, mapped non-cacheable.", frames * FRAME / 1024, start);
    }
}

/// What the shared aarch64 files ask of the embedded kernel's `hal` module.
pub mod hal {
    /// True on a Raspberry Pi 4, recognised by its GIC-400 distributor address.
    pub fn is_pi4() -> bool {
        crate::gic::bases().0 == 0xff84_1000
    }

    /// Interrupts taken so far, for drivers that wait for one.
    pub fn irq_ticks() -> u64 {
        crate::gic::irq_ticks()
    }

    /// Routes interrupt `intid` to `handler` (level or edge triggered) on the boot CPU.
    pub fn register_irq(intid: u32, level: bool, handler: fn(u32)) {
        crate::gic::register(intid, level, handler)
    }
}

// The Pi 4's on-board Ethernet (Broadcom GENET v5): the embedded kernel's driver, a singleton over the DMA pool.
#[path = "../../../../kernel/src/genet.rs"]
pub mod genet;

/// The Genet as a `kernel_core::nic::PlatformNic`.
pub struct GenetNic;

impl kernel_core::nic::PlatformNic for GenetNic {
    fn family(&self) -> &'static str {
        "genet"
    }
    fn mac(&self) -> [u8; 6] {
        genet::mac().unwrap_or([0; 6])
    }
    fn link_up(&self) -> bool {
        genet::info().is_some_and(|i| i.link_up)
    }
    fn link_speed(&self) -> Option<(u16, bool)> {
        genet::info().filter(|i| i.link_up).map(|i| (i.speed as u16, i.full_duplex))
    }
    fn receive(&self, out: &mut [u8]) -> Option<usize> {
        Some(genet::receive(out)).filter(|&n| n != 0)
    }
    fn send(&self, frame: &[u8]) -> bool {
        genet::send(frame)
    }
    fn set_promiscuous(&self, on: bool) {
        genet::set_promiscuous(on);
    }
    fn counters(&self) -> (u64, u64, u64) {
        genet::info().map_or((0, 0, 0), |i| (i.received.into(), i.sent.into(), i.dropped.into()))
    }
    fn describe(&self) {
        genet::details();
    }
}

// The Pi's SD host controller (EMMC2) or a PCI SDHCI (QEMU's `sdhci-pci`): the embedded kernel's driver, polled data port, no DMA.
#[path = "../../../../kernel/src/sdhci.rs"]
pub mod sdhci;

/// The crash machinery (record, reset) is not ported yet: a fatal exception is reported and the CPU parks.
pub mod crash {
    /// The timer interrupt's hook (after its acknowledgement): the boot CPU checks the watchdog, every CPU runs the scheduler's tick.
    pub fn tick() {
        if crate::percpu::this().index == 0 && crate::debug::watchdog_expired() {
            let elr: u64;
            unsafe { core::arch::asm!("mrs {}, elr_el1", out(reg) elr, options(nomem, nostack)) };
            fatal(super::crashrec::WATCHDOG_VECTOR, 0, elr, 0, true);
        }
        crate::sched::tick();
    }

    /// A fatal exception in the kernel (or the watchdog): report, record, then reset after `reboot-delay` if `reboot-on-crash`
    /// is set (always for the watchdog) unless the loader saw too many boots that never reached the milestone; else halt.
    pub fn fatal(kind: u64, esr: u64, elr: u64, far: u64, always_reset: bool) -> ! {
        crate::cpu::interrupts_disable();
        crate::println!("*** fatal exception {kind}: ESR {esr:#x}, ELR {elr:#x}, FAR {far:#x}.");
        super::crashrec::record(kind, esr, elr, far);
        if crate::config::reboot_on_crash() || always_reset {
            if crate::debug::crash_loop_tripped() {
                crate::hwwd::stop();
                crate::println!("*** crash loop: {} boots in a row never reached the milestone; not resetting again. Halting for an operator.", crate::debug::boot_crash_count());
            } else {
                crate::println!("*** resetting in {} s", crate::config::reboot_delay());
                crate::hwwd::reload();
                crate::timer::busy_wait_ms(u64::from(crate::config::reboot_delay()) * 1000);
                crate::power::crash_reset();
            }
        }
        crate::hwwd::stop();
        crate::println!("*** the kernel stops here");
        crate::halt()
    }
}

pub mod slots {
    pub fn mark_clean_stop() {}
}

pub mod trap;
pub mod smp;
pub mod usermode;
pub mod user_blobs;

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

/// `debug crash`: a deliberate fault (an undefined instruction).
pub fn crash_now() {
    unsafe { core::arch::asm!("udf #0") };
}

pub fn halt() -> ! {
    cpu::halt()
}

pub fn wait_for_interrupt() {
    cpu::sleep()
}

pub use timer::TICK_HZ;

pub fn ticks() -> u64 {
    gic::irq_ticks()
}

pub fn micros() -> u64 {
    timer::micros()
}

/// A free-running counter, only good for mixing into seeds and timing short spans (the generic counter).
pub fn cycle_counter() -> u64 {
    timer::counter()
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
    // EL0 tapes need the EL1&0 regime: a firmware that enters at EL2 (a Pi 4) is left behind here.
    paging::drop_to_el1_when_loaded();
    paging::build_and_load(info, &info.framebuffer);
    mmu::share_kernel_slots();
    paging::map_device(pl011::base(), 4096);
    println!("Page tables: loaded; RAM no-execute in 2 MiB blocks, kernel image W^X, page 0 and the stack guard unmapped.");
    unsafe { &*(info as *const BootInfo) }
}

pub fn init_interrupts() {
    activate(0);
    gic::init();
    usermode::init_cpu();
}

/// Interrupts on, and the timer ticking; the century register is an x86 RTC detail.
pub fn init_machine(_machine: &boot::Machine) -> u8 {
    cpu::interrupts_enable();
    let start = timer::micros();
    while timer::micros() - start < 100_000 {
        cpu::sleep();
    }
    let smp = smp::start_all(acpi::rsdp());
    println!("SMP: {} CPU(s) online ({} other started, {} failed).", crate::percpu::count(), smp.started, smp.failed);
    println!("GIC at {:#x}/{:#x}: {} timer interrupts (INTID 30) in 100 ms, expected about 10; counter {} Hz.", gic::bases().0, gic::bases().1, gic::irq_ticks(), timer::frequency());
    0
}
