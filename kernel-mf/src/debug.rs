//! Unattended operation for the server kernel, the embedded kernel's `debug.rs` redone for it: the crash record, `reboot-on-crash`
//! with the crash-loop guard, and the watchdog. The slot metadata (A/B fallback) is `slots.rs`.
//!
//! **The crash record** is one page of RAM at physical 0x5000 (below the 1 MiB the allocator never hands out; the BIOS stage two
//! leaves it alone). The fatal-exception handler writes it, the next boot prints and clears it. RAM does not survive a reset on
//! every machine; then there is simply nothing to report.
//!
//! **The watchdog**: the service thread calls [`pet`] on every pass. The boot CPU's timer interrupt checks that the last pet is
//! younger than N seconds; if not, the machine is hung (a CPU 0 with interrupts off never reaches this check either, which only a
//! hardware watchdog could catch), so it records a crash (vector 240, the RIP the CPU was at) and resets. Off after three unclean
//! boots in a row, and the streak is cleared by a boot that runs five minutes (`slots.rs`).

use core::sync::atomic::{AtomicU64, Ordering};

use crate::trap::Frame;

pub const CRASH_RECORD_ADDRESS: u64 = 0x5000;
const CRASH_MAGIC: u64 = u64::from_le_bytes(*b"FOSCRASH");
pub const WATCHDOG_VECTOR: u64 = 240;

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

static mut BOOT_CRASH_COUNT: u32 = 0;
/// Where the record page is in the direct map, 0 when it is not mapped (no RAM record).
static mut RECORD: u64 = 0;
static LAST_PET: AtomicU64 = AtomicU64::new(0);
/// Watchdog limit in ticks, 0 while off.
static LIMIT_TICKS: AtomicU64 = AtomicU64::new(0);

pub fn init(crash_count: u32) {
    unsafe {
        BOOT_CRASH_COUNT = crash_count;
        RECORD = crate::vm::writable_physical(CRASH_RECORD_ADDRESS).unwrap_or(0);
    }
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
        ticks: crate::apic::ticks(),
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

/// The loader counted this many consecutive boots that never reached the milestone: stop rebooting.
pub fn crash_loop_tripped() -> bool {
    unsafe { BOOT_CRASH_COUNT >= u32::from(fosfor_abi::MAX_CONSECUTIVE_CRASHES) }
}

pub fn boot_crash_count() -> u32 {
    unsafe { BOOT_CRASH_COUNT }
}

/// The service thread is making progress.
pub fn pet() {
    LAST_PET.store(crate::apic::ticks(), Ordering::Relaxed);
}

/// Arms the watchdog at the milestone (`unclean` boots in a row keep it off).
pub fn arm(seconds: Option<u32>, unclean: u8) {
    let Some(seconds) = seconds else { return };
    if unclean >= 3 {
        println!("Watchdog stays off: {unclean} unclean boots in a row (a hang must not become a reboot loop).");
        println!("It comes back on the next boot once this one has run five minutes.");
        return;
    }
    let seconds = seconds.max(5);
    pet();
    LIMIT_TICKS.store(u64::from(seconds) * crate::apic::TICK_HZ, Ordering::Relaxed);
    println!("Watchdog armed: reset after {seconds} s without progress.");
}

pub fn watchdog_seconds() -> Option<u64> {
    match LIMIT_TICKS.load(Ordering::Relaxed) {
        0 => None,
        ticks => Some(ticks / crate::apic::TICK_HZ),
    }
}

/// The boot CPU's timer interrupt: resets a machine whose service thread has stopped petting.
pub fn watchdog_check(frame: &mut Frame) {
    let limit = LIMIT_TICKS.load(Ordering::Relaxed);
    if limit != 0 && crate::apic::ticks().saturating_sub(LAST_PET.load(Ordering::Relaxed)) > limit {
        LIMIT_TICKS.store(0, Ordering::Relaxed);
        frame.vector = WATCHDOG_VECTOR;
        crate::trap::fatal_reset(frame);
    }
}
