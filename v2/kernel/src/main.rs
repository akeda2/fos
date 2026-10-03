//! fos v2 kernel, milestone M3: PCI, an e1000 NIC and the network terminal (on top of
//! M2, memory and interrupts).
//!
//! On top of M1 (loader handoff, own GDT, serial and framebuffer console) the kernel
//! now owns the machine's memory and its interrupts:
//!
//! * a physical page allocator built from the firmware's memory map;
//! * its own page tables with NX, read-only text and unmapped guard pages;
//! * a TSS-backed IDT, so a fault is reported in words (and a stack overflow, which
//!   takes down the normal stack, still gets a clean report on a stack of its own);
//! * ACPI tables, the local APIC and a calibrated 100 Hz timer tick;
//! * a clean power-off through the UEFI runtime services.
//!
//! See docs/v2-design.md, section 11.
#![no_std]
#![no_main]

mod acpi;
mod ahci;
mod block;
mod apic;
mod config;
mod debug;
mod cpu;
mod e1000;
mod ehci;
mod fault;
mod fosfs;
mod font_data;
mod gpt;
mod framebuffer;
mod gdt;
mod idt;
mod ioapic;
mod keyboard;
mod log;
mod memory;
mod msi;
mod paging;
mod pci;
mod power;
mod serial;
mod storage;
mod tape;
mod term;
mod usb_hid;
mod usb_storage;
mod xhci;

use core::fmt::Write;
use core::panic::PanicInfo;
use fault::Frame;
use fos_v2_abi::{BootInfo, PIXEL_RGB};

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
core::arch::global_asm!(include_str!("isr_stubs.S"));

/// `println!` goes to the serial port, the boot log and, once it exists, the framebuffer.
pub fn print(arguments: core::fmt::Arguments) {
    struct Both;
    impl Write for Both {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let _ = serial::Serial.write_str(text);
            log::push(text);
            framebuffer::write_str(text);
            term::relay(text);
            Ok(())
        }
    }
    let _ = Both.write_fmt(arguments);
}

#[macro_export]
macro_rules! println {
    () => { $crate::print(format_args!("\n")) };
    ($($argument:tt)*) => { $crate::print(format_args!("{}\n", format_args!($($argument)*))) };
}

static BREAKPOINTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Every interrupt and exception arrives here, from `isr_common` (isr_stubs.S).
#[unsafe(no_mangle)]
extern "sysv64" fn interrupt_dispatch(frame: &mut Frame) {
    match frame.vector {
        3 => {
            BREAKPOINTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            println!("Breakpoint exception at {:#x}: handled, execution continues.", frame.rip);
        }
        v if v == u64::from(apic::TIMER_VECTOR) => {
            apic::tick();
            debug::watchdog_tick(frame);
        }
        v if msi::handles(v) => msi::dispatch(v),
        v if v == u64::from(keyboard::KEYBOARD_VECTOR) => {
            keyboard::interrupt();
            apic::end_of_interrupt();
        }
        v if v == u64::from(apic::SPURIOUS_VECTOR) => {} // a spurious interrupt needs no EOI
        v if v < 32 => fault::fatal(frame),
        v => println!("Unexpected interrupt vector {v}."),
    }
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn kmain(info: *const BootInfo) -> ! {
    serial::init();
    println!();
    println!("FOS v2 kernel (milestone M4): running in 64-bit long mode.");

    let info = unsafe { &*info };
    if !info.is_valid() {
        println!("BootInfo is not valid (magic {:#x}, version {}): stopping.", info.magic, info.version);
        cpu::halt();
    }
    power::set_runtime_services(info.runtime_services);
    tape::set_boot_info(info);
    println!("BootInfo: version {}, kernel at {:#x} ({} KiB), booted from slot {}.", info.version, info.kernel_base, info.kernel_size / 1024, info.boot_slot);
    let fb = &info.framebuffer;
    println!(
        "Framebuffer: {}x{} {} at {:#x}. ACPI RSDP at {:#x}.",
        fb.width,
        fb.height,
        if fb.format == PIXEL_RGB { "RGB" } else { "BGR" },
        fb.base,
        info.acpi_rsdp
    );

    // ---- physical memory ----
    let map_bytes = info.memory_map_count * info.memory_map_descriptor_size;
    memory::init(
        info,
        &[
            (info.kernel_base, info.kernel_size),
            (info as *const BootInfo as u64, 4096),
            (info.memory_map, map_bytes),
            (info.acpi_rsdp & !0xfff, 8192),
        ],
    );
    let stats = memory::stats();
    println!("Physical memory: {} MiB managed, {} MiB free (one bitmap, one pool).", stats.frames * memory::FRAME / (1024 * 1024), stats.free * memory::FRAME / (1024 * 1024));
    self_test_allocator();

    // ---- descriptor tables, then page tables ----
    let layout = paging::layout();
    let double_fault_stack_top = unsafe {
        unsafe extern "C" {
            static __ist_top: u8;
        }
        core::ptr::addr_of!(__ist_top) as u64
    };
    gdt::load(double_fault_stack_top);
    idt::load();
    println!("GDT + TSS loaded (double-fault stack at {:#x}); IDT loaded: 256 gates.", double_fault_stack_top);
    paging::build_and_load(info, fb);
    println!(
        "Paging: own page tables at {:#x}; NX and write-protect on; text {:#x}-{:#x} read+execute, rodata read-only, data/bss/stacks no-execute; page 0 and stack guard pages unmapped.",
        paging::root(),
        layout.text.0,
        layout.text.1
    );

    debug::init(info); // after paging: it checks that the crash-record page is mapped
    if framebuffer::init(fb) {
        framebuffer::write_str(log::contents());
        let (columns, rows) = framebuffer::size().unwrap_or((0, 0));
        println!(
            "Framebuffer console ready: {columns} columns x {rows} rows, mapped {}.",
            if paging::write_combining_enabled() { "write-combining" } else { "uncached" }
        );
        framebuffer::fill_rect(0, fb.height as usize - 48, 64, 32, 255, 0, 0);
        framebuffer::fill_rect(80, fb.height as usize - 48, 64, 32, 0, 255, 0);
        framebuffer::fill_rect(160, fb.height as usize - 48, 64, 32, 0, 0, 255);
    } else {
        println!("No framebuffer: serial console only.");
    }

    // ---- ACPI, interrupts and the clock ----
    let tables = acpi::open(info.acpi_rsdp);
    let (fadt, madt) = match &tables {
        Some(tables) => (acpi::fadt(tables), acpi::madt(tables)),
        None => (None, None),
    };
    match (&tables, &fadt, &madt) {
        (Some(tables), Some(fadt), Some(madt)) => {
            println!(
                "ACPI {}: PM timer at port {:#x} ({}-bit), local APIC at {:#x}, {} processor(s), {} I/O APIC(s).",
                if tables.revision >= 2 { "2.0+" } else { "1.0" },
                fadt.pm_timer_port,
                if fadt.pm_timer_32_bit { 32 } else { 24 },
                madt.local_apic_address,
                madt.processors,
                madt.io_apics
            );
        }
        _ => println!("ACPI: FADT/MADT not found; no timer tick."),
    }

    if let (Some(fadt), Some(madt)) = (&fadt, &madt) {
        match apic::start_timer(fadt, madt.local_apic_address) {
            Some(report) => {
                println!(
                    "Local APIC ({} mode) at {:#x}; timer clock {} MHz, ticking at {} Hz on vector {:#x}.",
                    if report.x2apic { "x2APIC" } else { "xAPIC" },
                    report.base,
                    report.timer_hz / 1_000_000,
                    apic::TICK_HZ,
                    apic::TIMER_VECTOR
                );
                cpu::interrupts_enable();
                self_test_interrupts(fadt);
            }
            None => println!("No usable local APIC or PM timer: no clock tick."),
        }
    }

    provoke_fault_if_requested();

    // ---- local keyboard (never a boot dependency: failures are reported and ignored) ----
    match madt.as_ref().and_then(ioapic::init) {
        Some(pins) => {
            println!("I/O APIC: {pins} pins; ISA interrupts routed through it.");
            match keyboard::init_ps2() {
                keyboard::Ps2::Ready { gsi } => println!("PS/2 keyboard: 8042 ready, scancode set 1, IRQ 1 on GSI {gsi} -> vector {:#x}.", keyboard::KEYBOARD_VECTOR),
                keyboard::Ps2::NoInterrupt => println!("PS/2 keyboard: controller present but its interrupt could not be routed; no PS/2 input."),
                keyboard::Ps2::Absent => println!("PS/2 keyboard: no 8042 controller; no PS/2 input."),
            }
        }
        None => println!("No I/O APIC: no PS/2 keyboard interrupt (USB keyboards and the network still work)."),
    }

    // ---- PCI and the network ----
    let count = pci::scan();
    println!("PCI: {count} functions found (`pci` lists them).");
    // ---- USB keyboards (xHCI) ----
    let controllers = xhci::init_all();
    let ehci_controllers = ehci::init_all();
    if controllers + ehci_controllers == 0 {
        println!("USB: no xHCI or EHCI controller.");
    } else {
        let keyboards = usb_hid::probe_all();
        println!("USB: {controllers} xHCI and {ehci_controllers} EHCI controller(s), {keyboards} usable device(s) (keyboards, disks).");
    }

    // ---- storage: find the boot disk by its partition GUID, mount FOSFS ----
    storage::init(info);
    if let Some(disk) = storage::boot_disk() {
        debug::set_payload(disk, info.payload_start);
    }
    debug::report_previous_crash();

    // ---- per-machine configuration files, then the start script ----
    config::load();

    // ---- network interfaces (after the settings file: `no-msi` decides how they take interrupts) ----
    let mut interfaces = 0;
    for device in pci::devices().filter(|d| d.vendor == e1000::VENDOR_INTEL && e1000::DEVICE_IDS.contains(&d.id)) {
        match e1000::E1000::init(&device, interfaces) {
            Ok(mut nic) => {
                println!("e1000 ({:?}) at {:02x}:{:02x}.{}: MAC {}, link {}.", nic.variant, device.bus, device.device, device.function, term::Mac(&nic.mac), if nic.link_up() { "up" } else { "down" });
                if config::no_msi() {
                    println!("  interrupts: polled (settings: no-msi).");
                } else {
                    match nic.enable_msi(&device) {
                        Ok(vector) => println!("  interrupts: MSI on vector {vector:#x} (the self-test message arrived); frames wake the CPU instead of being polled for."),
                        Err(reason) => println!("  interrupts: polled ({reason})."),
                    }
                }
                if term::install(nic, info.runtime_services).is_some() {
                    interfaces += 1;
                }
            }
            Err(error) => println!("e1000 at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    if interfaces == 0 {
        println!("No supported network interface found (this milestone drives the e1000 only).");
    }

    if config::debug().crash_on_boot {
        println!("debug: crash-on-boot is set; faulting now.");
        unsafe { core::arch::asm!("ud2") };
    }
    config::run_start(term::run_line);

    // The milestone: from here the machine can be used (locally or over the network).
    debug::confirm_boot(config::debug().watchdog_seconds);
    println!("FOS v2 kernel: M3 complete. Waiting for a term net client.");
    term::start_local();
    loop {
        debug::pet();
        debug::housekeeping();
        term::housekeeping();
        while let Some(key) = keyboard::pop() {
            term::local_input(key);
        }
        usb_hid::poll();
        term::poll();
        // Sleep until the next timer tick (or a NIC-less machine's only wakeup).
        unsafe { core::arch::asm!("hlt") };
    }
}

/// Allocates and frees frames, checking the bookkeeping and the contiguity and
/// address-limit promises.
fn self_test_allocator() {
    let before = memory::stats().free;
    let single = memory::allocate().expect("one frame");
    let run = memory::allocate_contiguous(8, None).expect("eight contiguous frames");
    let low = memory::allocate_contiguous(4, Some(1 << 32)).expect("four frames below 4 GiB");
    let zeroed = (0..(8 * memory::FRAME / 8)).all(|index| unsafe { (run as *const u64).add(index as usize).read() == 0 });
    let after_allocation = memory::stats().free;
    memory::free(single, 1);
    memory::free(run, 8);
    memory::free(low, 4);
    let after_free = memory::stats().free;
    let ok = zeroed && low + 4 * memory::FRAME <= (1 << 32) && before - after_allocation == 13 && after_free == before;
    println!(
        "Allocator self-test: {} (13 frames out and back, contiguous run zeroed, low frames below 4 GiB).",
        if ok { "ok" } else { "FAILED" }
    );
}

/// Takes a breakpoint (a handled exception) and measures the timer tick against the
/// ACPI PM timer, which is an independent clock.
fn self_test_interrupts(fadt: &acpi::Fadt) {
    unsafe { core::arch::asm!("int3") };
    println!("Exception self-test: {} breakpoint(s) delivered and handled.", BREAKPOINTS.load(core::sync::atomic::Ordering::Relaxed));

    let start_ticks = apic::ticks();
    let mask: u64 = if fadt.pm_timer_32_bit { 0xffff_ffff } else { 0x00ff_ffff };
    let pm_start = u64::from(cpu::inl(fadt.pm_timer_port)) & mask;
    while apic::ticks() - start_ticks < 100 {
        unsafe { core::arch::asm!("hlt") };
    }
    let pm_elapsed = (u64::from(cpu::inl(fadt.pm_timer_port)) & mask).wrapping_sub(pm_start) & mask;
    let milliseconds = pm_elapsed * 1000 / 3_579_545;
    println!("Timer self-test: 100 ticks took {milliseconds} ms by the PM timer (expected about 1000).");
}

/// The deliberate-fault builds (see Cargo.toml `[features]`): provoke one fault so the
/// exception report can be checked.
fn provoke_fault_if_requested() {
    #[cfg(feature = "fault-null")]
    unsafe {
        println!("Provoking a fault: reading the null page.");
        core::ptr::read_volatile(0x10 as *const u64);
    }
    #[cfg(feature = "fault-text-write")]
    unsafe {
        println!("Provoking a fault: writing to kernel code.");
        (paging::layout().text.0 as *mut u8).write_volatile(0);
    }
    #[cfg(feature = "fault-nx")]
    unsafe {
        println!("Provoking a fault: executing from a data page.");
        static mut CODE: [u8; 16] = [0xc3; 16];
        let function: extern "C" fn() = core::mem::transmute(core::ptr::addr_of!(CODE) as usize);
        function();
    }
    #[cfg(feature = "fault-ud")]
    unsafe {
        println!("Provoking a fault: an invalid opcode.");
        core::arch::asm!("ud2");
    }
    #[cfg(feature = "fault-stack")]
    {
        println!("Provoking a fault: unbounded recursion (kernel stack overflow).");
        #[inline(never)]
        fn recurse(depth: u64) -> u64 {
            let padding = [depth; 64];
            let below = recurse(depth + 1);
            below + core::hint::black_box(padding)[3]
        }
        core::hint::black_box(recurse(0));
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nFOS v2 kernel PANIC: {info}");
    cpu::halt()
}
