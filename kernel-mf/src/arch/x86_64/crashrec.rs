//! The crash record on x86: one page of RAM at physical 0x5000 (below the 1 MiB the allocator never hands out; the BIOS stage two
//! leaves it alone). The fatal-exception handler writes it, the next boot prints and clears it. RAM does not survive a reset on
//! every machine; then there is simply nothing to report. (The watchdog and the crash-loop guard that use it are `debug.rs`.)

use crate::trap::Frame;

pub const CRASH_RECORD_ADDRESS: u64 = 0x5000;
const CRASH_MAGIC: u64 = u64::from_le_bytes(*b"FOSCRASH");

#[repr(C)]
#[derive(Clone, Copy)]
struct CrashRecord {
    magic: u64,
    vector: u64,
    error: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
    ticks: u64,
    boot_slot: u64,
    registers: [u64; 8], // rax rbx rcx rdx rsi rdi rbp r8
    cr2: u64,
}

/// Where the record page is in the direct map, 0 when it is not mapped (no RAM record).
static mut RECORD: u64 = 0;

pub fn init() {
    unsafe { RECORD = crate::vm::writable_physical(CRASH_RECORD_ADDRESS).unwrap_or(0) };
    println!("Crash record: {} at physical {:#x} (survives a reset only if the firmware leaves RAM alone).", if unsafe { RECORD } != 0 { "RAM page" } else { "page not mapped, so no RAM record;" }, CRASH_RECORD_ADDRESS);
}

/// Writes the record for a fatal exception (no allocation: one page of mapped RAM).
pub fn record_crash(frame: &Frame) {
    let page = unsafe { RECORD };
    if page == 0 {
        return;
    }
    let record = CrashRecord {
        magic: CRASH_MAGIC,
        vector: frame.vector,
        error: frame.error,
        rip: frame.rip,
        cs: frame.cs,
        rflags: frame.rflags,
        rsp: frame.rsp,
        ss: frame.ss,
        ticks: crate::time::ticks(),
        boot_slot: u64::from(crate::slots::boot_slot()),
        registers: [frame.rax, frame.rbx, frame.rcx, frame.rdx, frame.rsi, frame.rdi, frame.rbp, frame.r8],
        cr2: crate::cpu::read_cr2(),
    };
    unsafe { (page as *mut CrashRecord).write_volatile(record) };
}

/// At boot: prints the previous boot's crash record, if there is a valid one, and clears it.
pub fn report_previous_crash() {
    let page = unsafe { RECORD };
    if page == 0 {
        return;
    }
    let record = unsafe { (page as *const CrashRecord).read_volatile() };
    if record.magic != CRASH_MAGIC {
        return;
    }
    unsafe { (page as *mut u64).write_volatile(0) };
    println!("Previous boot crashed (record found in RAM):");
    println!("  {} (vector {}), error code {:#x}, slot {}, {} ticks after boot", crate::trap::exception_name(record.vector), record.vector, record.error, record.boot_slot, record.ticks);
    println!("  RIP {:#018x}  CS {:#x}  RFLAGS {:#x}  RSP {:#018x}  SS {:#x}  CR2 {:#x}", record.rip, record.cs, record.rflags, record.rsp, record.ss, record.cr2);
    println!("  RAX {:#x} RBX {:#x} RCX {:#x} RDX {:#x} RSI {:#x} RDI {:#x} RBP {:#x} R8 {:#x}", record.registers[0], record.registers[1], record.registers[2], record.registers[3], record.registers[4], record.registers[5], record.registers[6], record.registers[7]);
}

