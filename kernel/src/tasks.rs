//! Background tapes ("services"): `run NAME ARGS &N` starts a tape that keeps running while the shell, or another tape,
//! is in use. See docs/design.md "Services".
//!
//! Cooperative, one CPU: the shell (or the foreground tape) is context 0 and each service has a stack of its own.
//! A context gives up the CPU only inside a tape call (`should_abort`, `read_key`, `micros`, `wait_for_event`), or in
//! the shell's idle loop, never inside a kernel routine, so the kernel needs no locks. Because a tape treats every
//! call as clobbering the SSE/AVX registers, switching there needs to save only the callee-saved integer registers.
//!
//! A service owns one NIC (`&N`, 1-based): the frames of that NIC are queued for it alone (the foreground tape's
//! `nic_receive` on that NIC returns nothing) and the card's multicast, VLAN, offload and stamp state is its own to
//! set and to clear when it ends. Console output is not redirected: a service should be quiet. Keys and `Escape` go
//! to the foreground tape only; `kill N` stops a service.

use core::sync::atomic::{AtomicUsize, Ordering};

use fosfor_abi::tape::TapeApi;

use crate::{memory, term, timer};

pub const MAX_SERVICES: usize = 2;
const STACK_BYTES: u64 = 1024 * 1024;
/// How long a context may run before a tape call hands the CPU on (a tape that idles hands it on at once).
const SLICE_US: u64 = 200;
/// Room at the top of a service's stack for its argument text.
const ARGS_BYTES: usize = 512;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Free,
    Live,
    /// Ended; its stack is freed by whichever context runs next.
    Dead,
}

struct Service {
    state: State,
    name: [u8; 16],
    name_length: usize,
    rsp: u64,
    stack: u64,
    nic: usize,
    abort: bool,
    /// Gave up the CPU in `wait_for_event`: nothing to do before the next interrupt.
    idle: bool,
}

impl Service {
    const EMPTY: Service = Service { state: State::Free, name: [0; 16], name_length: 0, rsp: 0, stack: 0, nic: 0, abort: false, idle: false };

    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_length]).unwrap_or("?")
    }
}

static mut SERVICES: [Service; MAX_SERVICES] = [Service::EMPTY; MAX_SERVICES];
/// Which context runs: 0 is the shell or foreground tape, `n` is service `n - 1`.
static mut CURRENT: usize = 0;
static mut MAIN_RSP: u64 = 0;
static mut SLICE_START: u64 = 0;
static LIVE: AtomicUsize = AtomicUsize::new(0);

fn services() -> &'static mut [Service; MAX_SERVICES] {
    unsafe { &mut *core::ptr::addr_of_mut!(SERVICES) }
}

/// How many services are running.
pub fn count() -> usize {
    LIVE.load(Ordering::Relaxed)
}

pub fn in_service() -> bool {
    unsafe { CURRENT != 0 }
}

/// True when a running service owns NIC `index` (0-based).
pub fn service_owns(index: usize) -> bool {
    count() != 0 && services().iter().any(|service| service.state == State::Live && service.nic == index)
}

/// The NIC of the service that is running now.
pub fn current_nic() -> Option<usize> {
    let current = unsafe { CURRENT };
    (current != 0).then(|| services()[current - 1].nic)
}

core::arch::global_asm!(
    ".global fos_ctx_switch",
    "fos_ctx_switch:",
    // rdi: where to save the old stack pointer, rsi: the new one.
    "push rbp",
    "push rbx",
    "push r12",
    "push r13",
    "push r14",
    "push r15",
    "mov [rdi], rsp",
    "mov rsp, rsi",
    "pop r15",
    "pop r14",
    "pop r13",
    "pop r12",
    "pop rbx",
    "pop rbp",
    "ret",
    ".global fos_task_entry",
    "fos_task_entry:",
    // First run of a service: r12 is the tape's entry, r13 its TapeApi.
    "mov rdi, r13",
    "and rsp, -16",
    "call r12",
    "call {exit}",
    "ud2",
    exit = sym task_exit,
);

unsafe extern "C" {
    fn fos_ctx_switch(save: *mut u64, load: u64);
    fn fos_task_entry();
}

fn saved_rsp(context: usize) -> u64 {
    unsafe { if context == 0 { MAIN_RSP } else { services()[context - 1].rsp } }
}

/// The next context after `from` that can run: the main one, or a live service.
fn next_runnable(from: usize) -> usize {
    for step in 1..=MAX_SERVICES + 1 {
        let candidate = (from + step) % (MAX_SERVICES + 1);
        if candidate == 0 || services()[candidate - 1].state == State::Live {
            return candidate;
        }
    }
    0
}

fn switch_to(next: usize) {
    unsafe {
        let from = CURRENT;
        if next == from {
            return;
        }
        let save: *mut u64 = if from == 0 { core::ptr::addr_of_mut!(MAIN_RSP) } else { &mut services()[from - 1].rsp };
        let load = saved_rsp(next);
        CURRENT = next;
        SLICE_START = timer::micros();
        fos_ctx_switch(save, load);
    }
    reap();
}

/// Frees the stacks of services that ended (it cannot be done from the stack in use).
fn reap() {
    for service in services().iter_mut() {
        if service.state == State::Dead {
            memory::free(service.stack, STACK_BYTES / memory::FRAME);
            service.state = State::Free;
        }
    }
}

fn yield_now() {
    switch_to(next_runnable(unsafe { CURRENT }));
}

/// Hands the CPU on if the running context has used its slice. Cheap enough for `micros`.
pub fn maybe_yield() {
    if count() != 0 && timer::micros().wrapping_sub(unsafe { SLICE_START }) >= SLICE_US {
        yield_now();
    }
}

/// From the shell's idle loop or a foreground tape's `wait_for_event`: lets every service run once. True when one is
/// still busy, so the caller must not sleep.
pub fn pass() -> bool {
    if count() == 0 || in_service() {
        return false;
    }
    yield_now();
    services().iter().any(|service| service.state == State::Live && !service.idle)
}

/// A service's `should_abort`.
pub fn service_should_abort() -> bool {
    maybe_yield();
    services()[unsafe { CURRENT } - 1].abort
}

/// A service's `read_key`: it has no keys, only "stop".
pub fn service_read_key() -> u32 {
    maybe_yield();
    u32::from(services()[unsafe { CURRENT } - 1].abort)
}

/// A service's `wait_for_event`: nothing to do until an interrupt, so the others may run.
pub fn service_wait() {
    term::service_poll();
    let me = unsafe { CURRENT } - 1;
    services()[me].idle = true;
    yield_now();
    services()[me].idle = false;
}

extern "C" fn task_exit() -> ! {
    let id = unsafe { CURRENT };
    let service = &mut services()[id - 1];
    term::nic_release(service.nic);
    crate::println!("Service {} ended.", service.name());
    service.state = State::Dead;
    LIVE.fetch_sub(1, Ordering::Relaxed);
    if count() == 0 && !term::tape_mode() {
        crate::tape::clear_fp_level();
    }
    let next = next_runnable(id);
    let mut unused = 0u64;
    unsafe {
        CURRENT = next;
        SLICE_START = timer::micros();
        fos_ctx_switch(&mut unused, saved_rsp(next));
    }
    unreachable!("a finished service was resumed");
}

/// `run NAME ARGS &N`: the staged file `name` (already read into the staging buffer) becomes a service on NIC `nic`
/// (0-based).
pub fn start(name: &str, arguments: &str, nic: usize) {
    if in_service() {
        return crate::println!("run: a service cannot start another.");
    }
    if nic >= term::nic_count() {
        return crate::println!("run: no NIC {} (this machine has {}).", nic + 1, term::nic_count());
    }
    if service_owns(nic) {
        return crate::println!("run: a service already owns NIC {}.", nic + 1);
    }
    if arguments.len() > ARGS_BYTES {
        return crate::println!("run: the arguments of a service take at most {ARGS_BYTES} bytes.");
    }
    let Some(slot) = services().iter().position(|service| service.state == State::Free) else {
        return crate::println!("run: {MAX_SERVICES} services are running already (`tasks`, `kill N`).");
    };
    let entry = match crate::tape::load_staged_service(slot + 1, name) {
        Ok(entry) => entry,
        Err(error) => return crate::println!("run: {name} is not a usable tape ({error:?})."),
    };
    let Some(stack) = memory::allocate_contiguous(STACK_BYTES / memory::FRAME, None) else {
        return crate::println!("run: no memory for the service's stack.");
    };
    let Some(info) = crate::tape::boot_info() else { return crate::println!("run: no boot info.") };
    // The top of the stack holds what the tape reads for as long as it runs: its API table and argument text.
    let args_at = (stack + STACK_BYTES - ARGS_BYTES as u64) & !15;
    let api_at = (args_at - core::mem::size_of::<TapeApi>() as u64) & !15;
    let bytes = arguments.as_bytes();
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), args_at as *mut u8, bytes.len());
        let text = core::str::from_utf8_unchecked(core::slice::from_raw_parts(args_at as *const u8, bytes.len()));
        (api_at as *mut TapeApi).write(crate::tape::build_api(info, text));
    }
    // What `fos_ctx_switch` pops (r15 r14 r13 r12 rbx rbp), then the address it returns to.
    let sp = api_at - 64;
    unsafe {
        let frame = sp as *mut u64;
        for word in 0..8 {
            frame.add(word).write(0);
        }
        frame.add(2).write(api_at);
        frame.add(3).write(entry);
        frame.add(6).write(fos_task_entry as *const () as u64);
    }
    let service = &mut services()[slot];
    *service = Service::EMPTY;
    let take = name.len().min(16);
    service.name[..take].copy_from_slice(&name.as_bytes()[..take]);
    service.name_length = take;
    service.rsp = sp;
    service.stack = stack;
    service.nic = nic;
    service.state = State::Live;
    LIVE.fetch_add(1, Ordering::Relaxed);
    term::clear_frame_queue_nic(nic);
    crate::tape::enable_fp_level();
    crate::println!("Service {name} started on NIC {}: `tasks` lists services, `kill {}` stops this one.", nic + 1, slot + 1);
    yield_now(); // let it start and print its first lines
}

/// `tasks`
pub fn list() {
    if count() == 0 {
        return crate::println!("No services running (`run NAME ARGS &N` starts one on NIC N).");
    }
    for (index, service) in services().iter().enumerate().filter(|(_, service)| service.state == State::Live) {
        let state = if service.abort { "stopping" } else if service.idle { "idle" } else { "running" };
        crate::println!("{}  {:<16} NIC {}  {state}", index + 1, service.name(), service.nic + 1);
    }
}

/// `kill N`
pub fn kill(argument: Option<&str>) {
    let Some(number) = argument.and_then(|word| word.parse::<usize>().ok()).filter(|n| (1..=MAX_SERVICES).contains(n)) else {
        return crate::println!("Usage: kill N   (N from `tasks`)");
    };
    let service = &mut services()[number - 1];
    if service.state != State::Live {
        return crate::println!("kill: no service {number}.");
    }
    service.abort = true;
    crate::println!("Stopping service {} ({}); it ends the next time it checks for Escape.", number, service.name());
    for _ in 0..4 {
        yield_now();
    }
}
