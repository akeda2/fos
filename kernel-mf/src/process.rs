//! Processes: ring 3 code in its own address space, one kernel thread each (the thread's kernel stack is where the process's
//! system calls and interrupts arrive). Slice 8b: no ELF loader or tape API yet; `spawn` takes a flat machine-code image that
//! starts at `USER_BASE`.
//!
//! Entering the kernel from ring 3:
//! * interrupts and exceptions use the TSS's rsp0 (the running thread's kernel stack, set by the scheduler at every switch) and
//!   the shared stubs, which `swapgs` when CS says ring 3 (`isr_stubs.S`, `tools/gen_isr_stubs.py --swapgs`);
//! * `syscall` (the fast path) lands in `syscall_entry`, which swaps GS the same way, switches to the kernel stack and builds
//!   the same frame an interrupt would, so one return path (`iretq`) serves both. SFMASK clears IF, DF, TF and AC.
//!
//! System calls: rax = number, rdi and rsi = arguments, result in rax (-1 for an unknown number or a bad pointer):
//! 0 `write(ptr, len)` to the console (at most 4096 bytes), 1 `exit(code)`, 2 `yield()`, 3 `sleep(ticks)`, 4 `ticks()`.
//!
//! What protects the kernel: user pages carry the U bit and live in PML4 slots 1..255 only; everything else is supervisor
//! (the kernel image's identity map in slot 0, the direct map and stacks in the upper half); SMEP stops the kernel from
//! executing user pages and SMAP from touching them except in the `user_access` windows (the three places that copy to or from a
//! process); a syscall pointer is checked page by page (`vm::user_range_ok`) before the kernel reads through it.
//! Not yet: FP/SSE state (user code that uses SSE faults with #UD, as the kernel never enables it), the guard page below
//! the user stack is implicit (nothing maps there; `make test-mf-boot` overflows it).

use alloc::boxed::Box;
use alloc::vec::Vec;

use crate::sync::SpinLock;
use crate::trap::Frame;
use crate::{cpu, pmm, sched, time, vm};
pub use crate::usermode::user_access;

/// Where a process's code is mapped, and the top of its stack. The stack sits just under the tape shim (`tapeapi::SHIM_BASE`,
/// 32 MiB up), with one unmapped page between them, so an image can be about 32 MiB.
pub const CODE_BASE: u64 = vm::USER_BASE;
pub const STACK_TOP: u64 = vm::USER_BASE + 0x1ff_f000;
/// 4 MiB: the audio tapes (strings and epiano need about 1.5 MiB or more, organ less) keep large buffers on the stack; 1 MiB
/// killed them with a page fault the moment they started.
const STACK_PAGES: u64 = 1024;
/// The stack's size; the page below it is never mapped (the code is at the bottom of the address space, the heap a gigabyte above
/// it), so running off the end is a page fault that kills only the process.
pub const STACK_BYTES: u64 = STACK_PAGES * 4096;
const WRITE_LIMIT: u64 = 4096;

pub use crate::sched::How;

#[derive(Clone, Copy)]
pub struct Outcome {
    pub pid: u64,
    pub how: How,
    pub written: u64,
}

static OUTCOMES: SpinLock<Vec<Outcome>> = SpinLock::new(Vec::new());

/// How processes have ended so far (the self-test collects these).
pub fn outcomes() -> Vec<Outcome> {
    OUTCOMES.lock().clone()
}

struct Launch {
    pid: u64,
    entry: u64,
    argument: u64,
    second: u64,
    stack: u64,
}

/// Starts a process running the machine code `image` (read-only, executable, at `CODE_BASE`) with a fresh stack. Returns the
/// thread id, or `None` when out of memory.
pub fn spawn(name: &str, image: &[u8]) -> Option<u64> {
    let root = vm::create_user_space()?;
    let pages = (image.len() as u64).div_ceil(4096).max(1);
    let built = (|| {
        for page in 0..pages {
            let frame = pmm::allocate(1, None)?;
            let start = (page * 4096) as usize;
            let end = image.len().min(start + 4096);
            unsafe { core::ptr::copy_nonoverlapping(image[start..end].as_ptr(), vm::direct(frame) as *mut u8, end - start) };
            crate::usermode::sync_code(vm::direct(frame), 4096);
            vm::map_user_page(root, CODE_BASE + page * 4096, frame, false, true).then_some(())?;
        }
        map_stack(root)
    })();
    start(name, root, built.map(|()| (CODE_BASE, 0, 0)))
}

/// Why `spawn_elf` refused.
#[derive(Debug)]
pub enum SpawnError {
    Elf(fosfor_abi::elf::Error),
    NoMemory,
    /// The image would reach the stack's guard page (`MAX_IMAGE` bytes at most).
    TooLarge,
}

/// The most address space an image may span: everything below the stack and the unmapped guard page under it.
pub const MAX_IMAGE: u64 = STACK_TOP - STACK_BYTES - 4096 - CODE_BASE;

/// Starts a process from a position-independent ELF64 executable (a tape): the segments are loaded at `CODE_BASE`, relocated
/// for it and mapped with the permissions of their program headers (a page two segments share gets the union, so it can end up
/// writable and executable; the tape linker script page-aligns segments). The tape is entered through the shim (`tapeapi`) with a
/// `TapeApi` whose `args` are `args`; when its entry returns the process exits with code 0.
pub fn spawn_elf(name: &str, image: &[u8], args: &[u8]) -> Result<u64, SpawnError> {
    let span = fosfor_abi::elf::span(image).map_err(SpawnError::Elf)? as u64;
    if span > MAX_IMAGE {
        return Err(SpawnError::TooLarge);
    }
    let pages = span / 4096;
    // Per-page permissions: bit 0 execute, bit 1 write.
    let mut flags = alloc::vec![0u8; pages as usize];
    fosfor_abi::elf::segments(image, |address, size, segment| {
        let first = address / 4096;
        let last = (address + size).div_ceil(4096);
        for page in first..last.min(pages) {
            flags[page as usize] |= (segment & 1) as u8 | ((segment & 2) as u8);
        }
    })
    .map_err(SpawnError::Elf)?;
    let frames = pmm::allocate(pages, None).ok_or(SpawnError::NoMemory)?;
    let entry = match unsafe { fosfor_abi::elf::load_for(image, vm::direct(frames), CODE_BASE) } {
        Ok(entry) => entry,
        Err(error) => {
            pmm::free(frames, pages);
            return Err(SpawnError::Elf(error));
        }
    };
    crate::usermode::sync_code(vm::direct(frames), pages * 4096);
    let Some(root) = vm::create_user_space() else {
        pmm::free(frames, pages);
        return Err(SpawnError::NoMemory);
    };
    // From here the frames belong to the address space: `destroy_user_space` frees every mapped leaf, so map each page or free it.
    let mut mapped = 0;
    let built = (|| {
        for page in 0..pages {
            let (execute, write) = (flags[page as usize] & 1 != 0, flags[page as usize] & 2 != 0);
            vm::map_user_page(root, CODE_BASE + page * 4096, frames + page * 4096, write, execute).then_some(())?;
            mapped += 1;
        }
        map_stack(root)?;
        crate::tapeapi::map_support(root, args)
    })();
    if built.is_none() {
        pmm::free(frames + mapped * 4096, pages - mapped);
    }
    start(name, root, built.map(|()| (crate::tapeapi::SHIM_BASE, crate::tapeapi::TABLE_BASE, entry))).ok_or(SpawnError::NoMemory)
}

fn map_stack(root: u64) -> Option<()> {
    for page in 0..STACK_PAGES {
        let frame = pmm::allocate(1, None)?;
        vm::map_user_page(root, STACK_TOP - (page + 1) * 4096, frame, true, false).then_some(())?;
    }
    Some(())
}

/// NICs (0-based mask) the next process started owns from its first instruction: a background tape (`run NAME &N`).
pub static START_NICS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

static NEXT_PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

fn start(name: &str, root: u64, entry: Option<(u64, u64, u64)>) -> Option<u64> {
    let Some((entry, argument, second)) = entry else {
        vm::destroy_user_space(root);
        return None;
    };
    let pid = NEXT_PID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    crate::tapenet::reserve(pid, START_NICS.swap(0, core::sync::atomic::Ordering::AcqRel));
    let launch = Box::into_raw(Box::new(Launch { pid, entry, argument, second, stack: STACK_TOP })) as usize;
    let id = sched::spawn_user(name, user_thread, launch, root);
    if id.is_none() {
        crate::tapenet::process_ended(pid);
        unsafe { drop(Box::from_raw(launch as *mut Launch)) };
        vm::destroy_user_space(root);
    }
    id.map(|_| pid)
}

extern "C" fn user_thread(launch: usize) {
    let launch = unsafe { Box::from_raw(launch as *mut Launch) };
    sched::set_pid(launch.pid);
    let (entry, argument, second, stack) = (launch.entry, launch.argument, launch.second, launch.stack);
    drop(launch);
    crate::usermode::enter(entry, stack, argument, second)
}

/// The kernel thread of an extra user thread: its process (pid, address space) already exists.
extern "C" fn extra_thread(launch: usize) {
    let launch = unsafe { Box::from_raw(launch as *mut Launch) };
    let (entry, argument, second, stack) = (launch.entry, launch.argument, launch.second, launch.stack);
    drop(launch);
    crate::usermode::enter(entry, stack, argument, second)
}

/// Starts another thread in the calling process: it runs `entry(argument)` on the user stack ending at `stack_top`, through
/// the shim (which ends the thread when `entry` returns). Returns its tid, or `None` (too many threads, no memory).
pub fn spawn_thread(entry: u64, argument: u64, stack_top: u64) -> Option<u64> {
    let launch = Box::into_raw(Box::new(Launch { pid: sched::current_pid(), entry: crate::tapeapi::thread_start(), argument: entry, second: argument, stack: stack_top })) as usize;
    let tid = sched::spawn_thread("thread", extra_thread, launch, sched::current_root(), sched::current_pid());
    if tid.is_none() {
        unsafe { drop(Box::from_raw(launch as *mut Launch)) };
    }
    tid
}

/// Ends the calling process: records how, gives its address space back and ends the thread. Interrupts must be off or
/// the caller must be in an exception or system call (its own kernel stack).
pub fn terminate(how: How) -> ! {
    cpu::interrupts_disable();
    // Any thread ending the process ends all of them; the last one out tears it down.
    sched::end_process(how);
    leave(Some(how))
}

/// The calling thread ends on its own (its entry returned); the process goes on until its last thread is gone.
pub fn thread_exit() -> ! {
    cpu::interrupts_disable();
    leave(None)
}

fn leave(how: Option<How>) -> ! {
    let thread = sched::current_pid();
    let root = sched::detach_root();
    let Some((how, written)) = sched::leave_process(how) else { sched::exit() };
    if SCREEN_OWNER.compare_exchange(thread, 0, core::sync::atomic::Ordering::Relaxed, core::sync::atomic::Ordering::Relaxed).is_ok() {
        crate::fb::gfx_close();
        crate::fb::extra_close();
    }
    if root != 0 {
        vm::destroy_user_space(root);
    }
    OUTCOMES.lock().push(Outcome { pid: thread, how, written });
    match how {
        How::Exited(code) => println!("Process {thread} exited with code {code}."),
        How::Fault(vector, address) => println!("Process {thread} killed by exception {vector} (address {address:#x})."),
        How::Killed => println!("Process {thread} killed."),
    }
    crate::tapefs::process_ended(thread);
    crate::tapenet::process_ended(thread);
    crate::tapeaudio::process_ended(thread);
    crate::tapeserial::process_ended(thread);
    #[cfg(target_arch = "aarch64")]
    crate::tapepi::process_ended(thread);
    crate::net::tape_sessions_ended(thread);
    crate::shell::end_foreground(thread);
    sched::exit()
}

/// A CPU exception from ring 3: only the process dies.
pub fn fault(frame: &Frame) -> ! {
    let (class, address) = frame.fault();
    terminate(How::Fault(class, address))
}

/// Called on the way out of every interrupt or system call from ring 3.
pub fn check_killed() {
    if sched::current_killed() {
        terminate(How::Killed);
    }
}

pub fn user_bytes(address: u64, length: u64) -> Option<Vec<u8>> {
    user_bytes_up_to(address, length, WRITE_LIMIT)
}

/// As `user_bytes`, with the caller's own size limit (file data is larger than a console line).
pub fn user_bytes_up_to(address: u64, length: u64, limit: u64) -> Option<Vec<u8>> {
    if length > limit || !vm::user_range_ok(sched::current_root(), address, length, false) {
        return None;
    }
    let mut buffer = alloc::vec![0u8; length as usize];
    // The process's address space is loaded, and the range was just checked present and user-readable.
    user_access(|| unsafe { core::ptr::copy_nonoverlapping(address as *const u8, buffer.as_mut_ptr(), length as usize) });
    Some(buffer)
}

/// Copies `data` to the calling process's memory at `address`; false if the range is not user-writable.
pub fn copy_to_user(address: u64, data: &[u8]) -> bool {
    if !vm::user_range_ok(sched::current_root(), address, data.len() as u64, true) {
        return false;
    }
    // The process's address space is loaded, and the range was just checked present and user-writable.
    user_access(|| unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), address as *mut u8, data.len()) });
    true
}

/// Console output from a process: UTF-8 is passed through, anything else is shown lossily.
pub fn console_write(bytes: &[u8]) {
    print!("{}", alloc::string::String::from_utf8_lossy(bytes));
    sched::add_written(bytes.len() as u64);
}

/// A word from the process's memory, if that is mapped for it.
pub fn user_u64(address: u64) -> Option<u64> {
    let bytes = user_bytes(address, 8)?;
    Some(u64::from_le_bytes(bytes.try_into().ok()?))
}

static SCREEN_OWNER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The pid that has the pixel screen open, 0 when nobody does (`top`; a pid that no longer exists means the same).
pub fn screen_owner() -> u64 {
    SCREEN_OWNER.load(core::sync::atomic::Ordering::Relaxed)
}

/// The calling process now has the pixel screen: give it back when the process ends.
pub fn note_screen_open() {
    SCREEN_OWNER.store(sched::current_pid(), core::sync::atomic::Ordering::Relaxed);
}

/// A system call: the number and arguments are where the CPU's calling convention puts them (`Frame`); the result goes back
/// the same way.
pub fn syscall(frame: &mut Frame) {
    let number = frame.syscall_number();
    if number >= crate::tapeapi::CALL_BASE {
        crate::tapeapi::call(frame);
        return;
    }
    let (a, b) = (frame.arg(0), frame.arg(1));
    let result = match number {
        0 => match user_bytes(a, b) {
            Some(bytes) => {
                console_write(&bytes);
                bytes.len() as u64
            }
            None => u64::MAX,
        },
        1 => terminate(How::Exited(a as i64)),
        2 => {
            sched::yield_now();
            0
        }
        3 => {
            sched::sleep_ticks(a.min(1000));
            0
        }
        4 => time::ticks(),
        5 => thread_exit(),
        _ => u64::MAX,
    };
    frame.set_result(result);
}
