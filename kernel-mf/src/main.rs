//! The server kernel ("FOS-mf"), slice 1: the skeleton. It is entered by the same UEFI and BIOS loaders as the embedded
//! kernel, with the same `BootInfo`; it narrates over COM1, checks what the loader handed over, summarises the memory map
//! and halts. Memory management, SMP and the consoles follow in the slices of docs/big-kernel.md ("Stage 2 design").
#![no_std]
#![no_main]

use core::fmt::{self, Write};
use core::panic::PanicInfo;
use fosfor_abi::{BootInfo, MemoryDescriptor, RAM_LIKE_TYPES, TYPE_CONVENTIONAL};

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

const COM1: u16 = 0x3f8;

fn outb(port: u16, value: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

/// 115200 baud, 8N1, FIFOs on: what the rest of this project assumes.
fn serial_init() {
    outb(COM1 + 1, 0x00); // no interrupts
    outb(COM1 + 3, 0x80); // DLAB on
    outb(COM1, 0x01); // divisor 1 = 115200 baud
    outb(COM1 + 1, 0x00);
    outb(COM1 + 3, 0x03); // 8N1, DLAB off
    outb(COM1 + 2, 0xc7); // FIFO on, cleared
    outb(COM1 + 4, 0x03); // DTR + RTS
}

struct Serial;

impl Write for Serial {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                put(b'\r');
            }
            put(byte);
        }
        Ok(())
    }
}

fn put(byte: u8) {
    // Bounded wait for the transmit register: a machine with no UART reads 0xff and must not hang the kernel.
    for _ in 0..100_000 {
        if inb(COM1 + 5) & 0x20 != 0 {
            break;
        }
    }
    outb(COM1, byte);
}

macro_rules! println {
    ($($argument:tt)*) => {{ let _ = writeln!(Serial, $($argument)*); }};
}

fn halt() -> ! {
    loop {
        unsafe { core::arch::asm!("cli", "hlt", options(nomem, nostack)) };
    }
}

#[unsafe(no_mangle)]
pub extern "sysv64" fn kmain(info: *const BootInfo) -> ! {
    serial_init();
    println!();
    println!("Fosfor OS server kernel (mf, slice 1): running in 64-bit long mode.");

    let info = unsafe { &*info };
    if !info.is_valid() {
        println!("BootInfo is not valid (magic {:#x}, version {}): stopping.", info.magic, info.version);
        halt();
    }
    println!("BootInfo: version {}, kernel at {:#x} ({} KiB), booted from slot {}.", info.version, info.kernel_base, info.kernel_size / 1024, info.boot_slot);
    let fb = &info.framebuffer;
    println!("Framebuffer: {}x{} at {:#x}. ACPI RSDP at {:#x}.", fb.width, fb.height, fb.base, info.acpi_rsdp);

    let (mut regions, mut ram_pages, mut free_pages) = (0u64, 0u64, 0u64);
    for MemoryDescriptor { kind, pages, .. } in info.memory_descriptors() {
        regions += 1;
        if RAM_LIKE_TYPES.contains(&kind) {
            ram_pages += pages;
        }
        if kind == TYPE_CONVENTIONAL {
            free_pages += pages;
        }
    }
    println!("Memory map: {regions} regions, {} MiB of RAM-like memory, {} MiB conventional.", ram_pages * 4096 >> 20, free_pages * 4096 >> 20);
    println!("kernel-mf: skeleton reached the end of boot; halting.");
    halt();
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!("\nFosfor OS server kernel PANIC: {info}");
    halt()
}
