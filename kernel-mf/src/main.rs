//! The server kernel ("FOS-mf"). It is entered by the same UEFI and BIOS loaders as the embedded kernel, with the same
//! `BootInfo`; it narrates over COM1. Slice 2 adds memory: the frame allocator, the kernel's own page tables (direct map,
//! W^X image) and the heap. SMP and the consoles follow in the slices of docs/big-kernel.md ("Stage 2 design").
#![no_std]
#![no_main]

// One crate root, two entry files (as in the embedded kernel): each declares its own modules, so every driver says
// `crate::vm`, `crate::sched`, `crate::println!` whatever the CPU. The x86-64 file is the server kernel as it was; the
// aarch64 one is the Raspberry Pi 4 / QEMU virt port, growing module by module (docs/big-kernel.md "mf on aarch64").
#[cfg(target_arch = "x86_64")]
include!("main_x86_64.rs");
#[cfg(target_arch = "aarch64")]
include!("main_aarch64.rs");
