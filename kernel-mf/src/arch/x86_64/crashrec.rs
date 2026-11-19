//! The crash record on x86: one page of RAM at physical 0x5000 (below the 1 MiB the allocator never hands out; the BIOS stage two
//! leaves it alone). The fatal-exception handler writes it, the next boot prints and clears it. RAM does not survive a reset on
//! every machine; then there is simply nothing to report. (The watchdog and the crash-loop guard that use it are `debug.rs`.)

use crate::trap::Frame;
use core::fmt::Write;

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

/// The debug tools' part of the record, further into the same page (`record_crash` rewrites only the first part): the
/// crashing code's backtrace and where the other CPUs were. Same lifetime as the main record.
const EXTRA_OFFSET: u64 = 512;
const EXTRA_MAGIC: u64 = u64::from_le_bytes(*b"FOSCRSH2");
const EXTRA_CPUS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy)]
struct Extra {
    magic: u64,
    /// The crashed kernel's image range, so its addresses print as image offsets (`k+...`) for `addr2line`.
    image: (u64, u64),
    depth: u64,
    frames: [u64; crate::debugcpu::FRAMES],
    /// CPUs that answered the capture (bit per CPU, up to `EXTRA_CPUS`), and where each was.
    answered: u64,
    pcs: [u64; EXTRA_CPUS],
    threads: [u32; EXTRA_CPUS],
}

const _: () = assert!(EXTRA_OFFSET as usize >= core::mem::size_of::<CrashRecord>());
const _: () = assert!(EXTRA_OFFSET as usize + core::mem::size_of::<Extra>() <= 4096);

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

/// The backtrace and the other CPUs' states (`debugcpu::report_crash`).
pub fn record_extra(frames: &[u64], answered: u128) {
    let page = unsafe { RECORD };
    if page == 0 {
        return;
    }
    let mut extra = Extra { magic: EXTRA_MAGIC, image: crate::debugcpu::image(), depth: frames.len() as u64, frames: [0; crate::debugcpu::FRAMES], answered: answered as u64 & 0xffff_ffff, pcs: [0; EXTRA_CPUS], threads: [0; EXTRA_CPUS] };
    extra.frames[..frames.len()].copy_from_slice(frames);
    for cpu in 0..EXTRA_CPUS {
        let snapshot = &crate::debugcpu::SNAPSHOTS[cpu];
        extra.pcs[cpu] = snapshot.pc.load(core::sync::atomic::Ordering::Relaxed);
        extra.threads[cpu] = snapshot.thread.load(core::sync::atomic::Ordering::Relaxed) as u32;
    }
    unsafe { ((page + EXTRA_OFFSET) as *mut Extra).write_volatile(extra) };
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
    let extra = unsafe { ((page + EXTRA_OFFSET) as *const Extra).read_volatile() };
    if extra.magic != EXTRA_MAGIC {
        return;
    }
    unsafe { ((page + EXTRA_OFFSET) as *mut u64).write_volatile(0) };
    // k+OFFSET is an offset into the crashed kernel's image: `addr2line -f -e fosfor-kernel-mf OFFSET` with that build.
    let code = |address: u64| crate::debugcpu::CodeIn(address, extra.image);
    let mut line = crate::debugcpu::Buffer::<200>::new();
    let _ = write!(line, "  backtrace:");
    for &frame in &extra.frames[..(extra.depth as usize).min(crate::debugcpu::FRAMES)] {
        let _ = write!(line, " {}", code(frame));
    }
    println!("{}", line.as_str());
    for cpu in (0..EXTRA_CPUS).filter(|&cpu| extra.answered & 1 << cpu != 0) {
        println!("  cpu{cpu}: thread {} at {}", extra.threads[cpu], code(extra.pcs[cpu]));
    }
}

