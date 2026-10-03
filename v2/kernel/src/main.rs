//! fos v2 kernel, milestone M1: reached from the loader in 64-bit long mode, it
//! checks what it was handed, installs its own GDT and page tables, brings up the
//! serial port and a framebuffer text console, and says so.
//!
//! The interesting part is what is *not* here: no real-mode trampoline, no 640 KiB
//! budget, no BIOS. The CPU is already in long mode with paging on when the first
//! instruction runs (the firmware did that), so the work is replacing the
//! firmware's state with the kernel's own. Later milestones add the physical
//! allocator, interrupts, PCI and the rest (docs/v2-design.md, section 11).
#![no_std]
#![no_main]

mod font_data;
mod framebuffer;
mod gdt;
mod log;
mod paging;
mod serial;

use core::fmt::Write;
use core::panic::PanicInfo;
use fos_v2_abi::{BootInfo, PIXEL_RGB, TYPE_CONVENTIONAL};

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

/// `println!` goes to the serial port and, once it exists, the framebuffer.
fn print(arguments: core::fmt::Arguments) {
    struct Both;
    impl Write for Both {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let _ = serial::Serial.write_str(text);
            log::push(text);
            framebuffer::write_str(text);
            Ok(())
        }
    }
    let _ = Both.write_fmt(arguments);
}

macro_rules! println {
    () => { print(format_args!("\n")) };
    ($($argument:tt)*) => { print(format_args!("{}\n", format_args!($($argument)*))) };
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn kmain(info: *const BootInfo) -> ! {
    serial::init();
    println!();
    println!("FOS v2 kernel (milestone M1): running in 64-bit long mode.");

    let info = unsafe { &*info };
    if !info.is_valid() {
        println!("BootInfo is not valid (magic {:#x}, version {}): stopping.", info.magic, info.version);
        halt();
    }
    println!(
        "BootInfo: version {}, kernel at {:#x} ({} KiB), booted from slot {}.",
        info.version,
        info.kernel_base,
        info.kernel_size / 1024,
        info.boot_slot
    );

    let (mut conventional, mut total_entries) = (0u64, 0);
    for descriptor in info.memory_descriptors() {
        total_entries += 1;
        if descriptor.kind == TYPE_CONVENTIONAL {
            conventional += descriptor.pages;
        }
    }
    println!("Memory map: {total_entries} entries, {} MiB of conventional (free) RAM.", conventional / 256);
    let fb = &info.framebuffer;
    println!(
        "Framebuffer: {}x{} {} at {:#x}, {} pixels per line.",
        fb.width,
        fb.height,
        if fb.format == PIXEL_RGB { "RGB" } else { "BGR" },
        fb.base,
        fb.stride
    );
    println!("ACPI RSDP at {:#x}; UEFI runtime services at {:#x}.", info.acpi_rsdp, info.runtime_services);

    println!(
        "CPU state from the firmware: CR0 {:#x}, CR4 {:#x}, EFER {:#x} (LME {}, LMA {}).",
        paging::read_cr0(),
        paging::read_cr4(),
        paging::read_efer(),
        (paging::read_efer() >> 8) & 1,
        (paging::read_efer() >> 10) & 1
    );

    gdt::load();
    println!("GDT loaded: kernel code selector {:#x}, data {:#x}.", gdt::KERNEL_CODE, gdt::KERNEL_DATA);

    let report = paging::build_and_load(info, fb);
    println!(
        "Paging: identity map of {} MiB RAM in 2 MiB pages (+{} skipped above 64 GiB), CR3 = {:#x}.",
        report.ram_pages * 2,
        report.skipped,
        report.cr3
    );

    if framebuffer::init(fb) {
        // Everything printed so far went to the serial port only: replay it.
        framebuffer::write_str(log::contents());
        let (columns, rows) = framebuffer::size().unwrap_or((0, 0));
        println!("Framebuffer console ready: {columns} columns x {rows} rows.");
        // Three colour bars in logical RGB, whatever the framebuffer's byte order:
        // if the format handling is right they look red, green and blue.
        framebuffer::fill_rect(0, fb.height as usize - 48, 64, 32, 255, 0, 0);
        framebuffer::fill_rect(80, fb.height as usize - 48, 64, 32, 0, 255, 0);
        framebuffer::fill_rect(160, fb.height as usize - 48, 64, 32, 0, 0, 255);
    } else {
        println!("No framebuffer: serial console only.");
    }

    println!("FOS v2 kernel: M1 complete. Halting.");
    halt()
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("hlt") };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nFOS v2 kernel PANIC: {info}");
    halt()
}
