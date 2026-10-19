use core::fmt::Write;
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, BOOT_INFO_MAGIC, BOOT_INFO_VERSION};

mod arch;

/// `println!` goes to the PL011 for now; the framebuffer and the boot log join it as their modules are ported.
pub fn print(arguments: core::fmt::Arguments) {
    let _ = arch::aarch64::pl011::Serial.write_fmt(arguments);
}

/// The loader jumps here (through `_start`, arch/aarch64/mod.rs) with the BootInfo pointer.
#[unsafe(no_mangle)]
pub extern "C" fn kmain(info: *const BootInfo) -> ! {
    println!();
    println!("Fosfor OS kernel (aarch64): running from the UEFI loader.");
    let info = unsafe { &*info };
    if info.magic != BOOT_INFO_MAGIC || info.version != BOOT_INFO_VERSION || info.size as usize != core::mem::size_of::<BootInfo>() {
        println!("BootInfo is not what this kernel expects (magic {:#x}, version {}, size {}).", info.magic, info.version, info.size);
    } else {
        println!(
            "BootInfo v{}: {} memory-map entries, kernel at {:#x} ({} KiB), slot {}, ACPI RSDP {:#x}.",
            info.version,
            info.memory_map_count,
            info.kernel_base,
            info.kernel_size / 1024,
            info.boot_slot,
            info.acpi_rsdp
        );
    }
    println!("Nothing more to do yet: parking the CPU.");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nKERNEL PANIC: {info}");
    loop {
        unsafe { core::arch::asm!("wfe") };
    }
}
