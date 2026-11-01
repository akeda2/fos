//! Kernel code shared by every core (docs/big-kernel.md, stage 1b). Modules reach the machine only through `fos_hal`; the
//! re-exports below let them keep saying `crate::block`, `crate::clock` and `crate::println!` as they did inside the kernel.

#![no_std]

pub use fos_hal::{block, clock, print, println, timer};

pub mod fosfs;
pub mod gpt;
pub mod lineedit;
