#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

core::arch::global_asm!(include_str!("entry.S"));

mod arch;
mod boot_info;
mod console;
mod cpu;
mod disk;
mod e1000;
mod fosfs;
mod gdt;
mod interrupts;
mod keyboard;
mod nic;
mod paging;
mod pci;
mod pmem;
mod rtl8139;
mod shell;
mod stack;
mod threecom;

use core::panic::PanicInfo;

use crate::arch::x86::halt_forever;
use crate::boot_info::BootInfo;

#[unsafe(no_mangle)]
pub extern "C" fn kmain(boot_info_address: u32) -> ! {
    let Some(boot_info) = (unsafe { BootInfo::from_address(boot_info_address) }) else {
        console::initialize(true);
        println!("KERNEL ERROR: invalid boot information.");
        halt_forever();
    };

    stack::initialize();
    console::initialize(boot_info.serial_available != 0);
    println!("\nKernel: protected-mode Rust entry reached.");
    println!("Kernel: validated loader boot information.");
    if paging::initialize().is_err() {
        println!("KERNEL ERROR: CPU lacks 4 MiB page support required for the paging foundation.");
        halt_forever();
    }
    println!(
        "Paging enabled: supervisor-only identity map for the low physical range and PCI MMIO; unmapped holes will fault."
    );
    pmem::initialize(boot_info);
    shell::run(boot_info)
}

#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    println!("\nKERNEL PANIC: {info}");
    halt_forever()
}
