//! Starting the other processors (the APs).
//!
//! The boot CPU wakes each AP with INIT, then a Startup IPI whose vector is a page number: the AP starts in 16-bit real mode
//! at `vector * 4096`, so the first code it runs must sit in the first MiB. That is the trampoline: real mode -> protected
//! mode -> long mode (paging on, with the kernel's own page tables), then a call into Rust on a stack the boot CPU gave it.
//!
//! The trampoline is assembled to run at the fixed address `TRAMPOLINE`; the image holds it, `start_all` copies it there.
//! Its page is identity-mapped in the kernel's tables only while APs are being started (the switch on of paging needs the
//! code to be reachable at the address it is running from). APs are started one at a time, so one data block serves all.
//!
//! The page is below 1 MiB, which the loaders hand over as loader data (BIOS) or conventional memory (UEFI). By the time the
//! kernel runs, nothing there is in use except the crash-record page at 0x5000, which `TRAMPOLINE` stays clear of.

use crate::{acpi, apic, boot::Machine, cpu, idt, percpu, pmm, vm};
use core::sync::atomic::Ordering;

pub const TRAMPOLINE: u64 = 0x8000;
const STACK_FRAMES: u64 = 8;
const IST_FRAMES: u64 = 2;

core::arch::global_asm!(
    r#"
    .section .rodata.trampoline, "a"
    .balign 16
    .global tramp_start
    .global tramp_end
    .global tramp_data
    .set TRAMP, 0x8000
    tramp_start:
    .code16
        cli
        cld
        movw %cs, %ax
        movw %ax, %ds
        lgdtl (tramp_gdtr - tramp_start)
        movl %cr0, %eax
        orl $1, %eax
        movl %eax, %cr0
        ljmpl $0x08, $(TRAMP + tramp_protected - tramp_start)
    .code32
    tramp_protected:
        movw $0x10, %ax
        movw %ax, %ds
        movw %ax, %es
        movw %ax, %ss
        movl %cr4, %eax
        orl $0x20, %eax                      # PAE: long mode needs it
        movl %eax, %cr4
        movl (TRAMP + tramp_data - tramp_start), %eax   # the kernel root, allocated below 4 GiB
        movl %eax, %cr3
        movl $0xc0000080, %ecx               # EFER
        rdmsr
        orl $0x900, %eax                     # LME | NXE
        wrmsr
        movl %cr0, %eax
        orl $0x80010001, %eax                # PG | WP | PE
        movl %eax, %cr0
        ljmpl $0x18, $(TRAMP + tramp_long - tramp_start)
    .code64
    tramp_long:
        movw $0x10, %ax
        movw %ax, %ds
        movw %ax, %es
        movw %ax, %ss
        movq (TRAMP + tramp_data + 8 - tramp_start), %rsp
        movq (TRAMP + tramp_data + 16 - tramp_start), %rdi
        movq (TRAMP + tramp_data + 24 - tramp_start), %rax
        xorl %ebp, %ebp
        callq *%rax
    1:  hlt
        jmp 1b
    .balign 8
    tramp_gdt:
        .quad 0
        .quad 0x00cf9a000000ffff             # 0x08: 32-bit code
        .quad 0x00cf92000000ffff             # 0x10: data
        .quad 0x00af9a000000ffff             # 0x18: 64-bit code
    tramp_gdtr:
        .word 31
        .long TRAMP + tramp_gdt - tramp_start
    .balign 8
    # root, stack top, argument (the CPU index), entry point: 8 bytes each, written by the boot CPU before each start
    tramp_data:
        .quad 0, 0, 0, 0
    tramp_end:
    .code64
    .text
    "#,
    options(att_syntax)
);

unsafe extern "C" {
    static tramp_start: u8;
    static tramp_end: u8;
    static tramp_data: u8;
}

/// Is `TRAMPOLINE`'s page ordinary memory according to the loader's map? (UEFI: conventional, boot-services or loader
/// memory; BIOS: its E820 usable RAM arrives as loader data.)
fn trampoline_usable(machine: &Machine) -> bool {
    machine.descriptors().any(|d| matches!(d.kind, 2 | 3 | 4 | 7) && d.physical_start <= TRAMPOLINE && TRAMPOLINE + 4096 <= d.physical_start + d.pages * 4096)
}

fn write_word(offset: u64, value: u64) {
    unsafe { (vm::direct(TRAMPOLINE + offset) as *mut u64).write_volatile(value) };
}

extern "sysv64" fn ap_main(index: u64) -> ! {
    let cpu = percpu::get(index as usize);
    percpu::activate(index as usize, cpu.ist_top);
    idt::load();
    crate::process::init_cpu();
    crate::fpu::init_cpu();
    cpu::pat_init();
    apic::enable_this_cpu();
    apic::start_local_timer();
    cpu.apic_id = apic::id();
    cpu.online.store(true, Ordering::Release);
    cpu::interrupts_enable();
    crate::sched::idle_loop()
}

pub struct Report {
    pub started: usize,
    pub failed: usize,
}

/// Starts every processor the MADT lists except the calling one. `cpus` gets an entry per CPU (index 0 is the boot CPU).
/// Each AP's stacks come from the NUMA node its APIC id belongs to (SRAT), falling back to any node.
pub fn start_all(machine: &Machine, madt: &acpi::Madt, srat: Option<&acpi::Srat>) -> Report {
    let boot_id = apic::id();
    let ranges: &[(u64, u64, u32)] = srat.map_or(&[], |s| &s.memory[..s.memory_count]);
    let node_of = |apic_id: u32| srat.map_or(0, |s| s.node_of_cpu(apic_id));

    let boot = percpu::get(0);
    boot.apic_id = boot_id;
    boot.node = node_of(boot_id);
    boot.online.store(true, Ordering::Release);

    let mut report = Report { started: 0, failed: 0 };
    let mut count = 1;
    let others = madt.cpus[..madt.cpu_count].iter().copied().filter(|&id| id != boot_id);
    if !trampoline_usable(machine) {
        println!("SMP: the page at {TRAMPOLINE:#x} is not ordinary memory in the loader's map: the other CPUs stay stopped.");
        percpu::set_count(1);
        return Report { started: 0, failed: others.count() };
    }

    let length = (&raw const tramp_end as u64) - (&raw const tramp_start as u64);
    let data = (&raw const tramp_data as u64) - (&raw const tramp_start as u64);
    assert!(length <= 4096);
    vm::map_device(TRAMPOLINE, 4096);
    unsafe { core::ptr::copy_nonoverlapping(&raw const tramp_start, vm::direct(TRAMPOLINE) as *mut u8, length as usize) };
    vm::identity_map_page(TRAMPOLINE);

    for apic_id in others {
        let index = count;
        if index >= percpu::MAX_CPUS {
            break;
        }
        let node = node_of(apic_id);
        let (Some(stack), Some(ist)) = (pmm::allocate_near(STACK_FRAMES, node, ranges), pmm::allocate_near(IST_FRAMES, node, ranges)) else {
            println!("SMP: no memory for the stacks of CPU {index}: skipped.");
            report.failed += 1;
            continue;
        };
        let cpu = percpu::get(index);
        cpu.apic_id = apic_id;
        cpu.node = node;
        cpu.ist_top = vm::direct(ist) + IST_FRAMES * 4096;
        write_word(data, vm::root());
        write_word(data + 8, vm::direct(stack) + STACK_FRAMES * 4096);
        write_word(data + 16, index as u64);
        write_word(data + 24, ap_main as *const () as u64);

        // INIT (level, assert), then two Startup IPIs: the second is the specified retry for an AP that missed the first.
        apic::send_ipi(apic_id, 0x4500);
        apic::busy_wait_ms(10);
        let vector = (TRAMPOLINE >> 12) as u32;
        apic::send_ipi(apic_id, 0x4600 | vector);
        apic::busy_wait_ms(1);
        apic::send_ipi(apic_id, 0x4600 | vector);
        let mut waited = 0;
        while !cpu.online.load(Ordering::Acquire) && waited < 200 {
            apic::busy_wait_ms(1);
            waited += 1;
        }
        if cpu.online.load(Ordering::Acquire) {
            report.started += 1;
            count += 1;
        } else {
            println!("SMP: CPU with APIC id {apic_id} did not start within 200 ms.");
            report.failed += 1;
        }
    }
    percpu::set_count(count);
    vm::identity_unmap_page(TRAMPOLINE);
    report
}
