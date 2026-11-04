//! Calls every function in the tape table with hostile arguments (null, tiny, unmapped, kernel-half, read-only, all-ones) and
//! checks the kernel survives. A tape pointer is only a number to the kernel: it must validate it, never follow it.
//! For the server kernel (FOS-mf) only: the embedded kernel trusts its tapes and would crash. `make test-mf-abibad`.
#![no_std]
#![no_main]

use core::mem::offset_of;
use tape_rt::{TapeApi, println};

tape_rt::tape_help!(
    br#"abibad server    call every tape-table function with bad pointers and sizes
  Only for the server kernel (mf); the embedded kernel trusts its tapes.
"#
);

type Call = extern "C" fn(usize, usize, usize, usize, usize, usize) -> usize;

/// Table words that are data, not functions.
const DATA: [usize; 6] = [
    0,
    offset_of!(TapeApi, args) / 8,
    offset_of!(TapeApi, args_len) / 8,
    offset_of!(TapeApi, memory_map) / 8,
    offset_of!(TapeApi, memory_map_count) / 8,
    offset_of!(TapeApi, memory_map_descriptor_size) / 8,
];

/// Functions a hostile argument could make harmful on purpose: formatting a disk.
const SKIP: [usize; 1] = [offset_of!(TapeApi, disk_format_exfat) / 8];

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if tape_rt::args() != b"server" {
        println!("abibad: for the server kernel only; run `abibad server`.");
        return;
    }
    let words = api as *const TapeApi as *const usize;
    let count = api.size as usize / 8;
    // The tape's own code is mapped, user-readable and not writable.
    let read_only = tape_main as *const () as usize;
    let unmapped = 0x1000;
    let kernel_half = 0xffff_8000_0000_0000;
    let mut calls = 0usize;
    let mut slots = 0usize;
    for index in 0..count {
        if DATA.contains(&index) || SKIP.contains(&index) {
            continue;
        }
        let call: Call = unsafe { core::mem::transmute(words.add(index).read()) };
        slots += 1;
        for bad in [0, 1, unmapped, kernel_half, read_only, usize::MAX] {
            // Pointer first, length second; length first; pointers everywhere; a pointer after two small numbers.
            for arguments in [
                [bad, 64, bad, 64, bad, 64],
                [64, bad, 64, bad, 64, bad],
                [bad, bad, bad, bad, bad, bad],
                [0, 0, bad, 64, bad, 64],
                [1, 1, bad, bad, 4096, bad],
            ] {
                call(arguments[0], arguments[1], arguments[2], arguments[3], arguments[4], arguments[5]);
                calls += 1;
            }
        }
    }
    println!("abibad: {calls} hostile calls to {slots} table functions, the kernel is still here.");
}
