//! Shared by the x86-64 and aarch64 kernels; only `ctx` (the register switch) and `service_host.rs` (what a service needs
//! from the NIC and tape code of its kernel) differ.
//!
//! Background tapes ("services"): `run NAME ARGS &N` starts a tape that keeps running while the shell, or another tape,
//! is in use. See docs/design.md "Services".
//!
//! Cooperative, one CPU: the shell (or the foreground tape) is context 0 and each service has a stack of its own.
//! A context gives up the CPU only inside a tape call (`should_abort`, `read_key`, `micros`, `wait_for_event`), or in
//! the shell's idle loop, never inside a kernel routine, so the kernel needs no locks. Because a tape treats every
//! call as clobbering the SSE/AVX registers, switching there needs to save only the callee-saved integer registers.
//!
//! A service owns NICs (`&N` or `&N,M,...`, 1-based; `&0` owns none): the frames of those NICs are queued for it alone (the foreground tape's
//! `nic_receive` on that NIC returns nothing) and the card's multicast, VLAN, offload and stamp state is its own to
//! set and to clear when it ends. Console output is not redirected: a service should be quiet. Keys and `Escape` go
//! to the foreground tape only; `kill N` stops a service.

use core::sync::atomic::{AtomicUsize, Ordering};

use crate::service_host as host;
use crate::{memory, timer};

pub const MAX_SERVICES: usize = 8;
/// NICs are a bit mask in `Service::nics` (bit i = NIC i + 1), so at most 64.
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
    nics: u64,
    abort: bool,
    /// Gave up the CPU in `wait_for_event`: nothing to do before the next interrupt.
    idle: bool,
}

impl Service {
    const EMPTY: Service = Service { state: State::Free, name: [0; 16], name_length: 0, rsp: 0, stack: 0, nics: 0, abort: false, idle: false };

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

/// Which context runs: 0 is the shell or foreground tape, `n` is service `n - 1`.
pub fn context() -> usize {
    unsafe { CURRENT }
}

/// Whether context `context` (1-based service number) is a running service.
pub fn context_live(context: usize) -> bool {
    context != 0 && services().get(context - 1).is_some_and(|service| service.state == State::Live)
}

pub fn in_service() -> bool {
    unsafe { CURRENT != 0 }
}

/// True when a running service owns NIC `index` (0-based).
pub fn service_owns(index: usize) -> bool {
    count() != 0 && index < 64 && services().iter().any(|service| service.state == State::Live && service.nics >> index & 1 != 0)
}

/// The first NIC of the service that is running now (what `nic_active` reports).
#[cfg_attr(target_arch = "aarch64", allow(dead_code))]
pub fn current_nic() -> Option<usize> {
    let current = unsafe { CURRENT };
    (current != 0).then(|| services()[current - 1].nics).filter(|&nics| nics != 0).map(|nics| nics.trailing_zeros() as usize)
}

/// Whether the running tape (a service, or the foreground) may drive NIC `index`: a service only its own NICs, the foreground
/// only those no service owns. The calls that send or change a card check this, so a tape cannot touch hardware it has not
/// reserved.
pub fn may_use(index: usize) -> bool {
    if in_service() { current_owns(index) } else { !service_owns(index) }
}

/// Whether the service that is running now owns NIC `index`.
pub fn current_owns(index: usize) -> bool {
    let current = unsafe { CURRENT };
    current != 0 && index < 64 && services()[current - 1].nics >> index & 1 != 0
}

#[cfg(target_arch = "x86_64")]
mod ctx {
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
        exit = sym super::task_exit,
    );

    unsafe extern "C" {
        fn fos_ctx_switch(save: *mut u64, load: u64);
        fn fos_task_entry();
    }

    pub unsafe fn switch(save: *mut u64, load: u64) {
        unsafe { fos_ctx_switch(save, load) }
    }

    /// The stack pointer a new service starts from: what `fos_ctx_switch` pops (r15 r14 r13 r12 rbx rbp), then the
    /// address it returns to. `below` is the lowest address the tape's API table and arguments occupy.
    pub unsafe fn first_frame(below: u64, entry: u64, api: u64) -> u64 {
        let sp = below - 64;
        let frame = sp as *mut u64;
        unsafe {
            for word in 0..8 {
                frame.add(word).write(0);
            }
            frame.add(2).write(api);
            frame.add(3).write(entry);
            frame.add(6).write(fos_task_entry as *const () as u64);
        }
        sp
    }
}

#[cfg(target_arch = "aarch64")]
mod ctx {
    // `fos_task_switch` (tape_aarch64.rs) saves x19-x30 and d8-d15; a first frame "returns" into `fos_service_entry`
    // with x19 = the tape's entry and x20 = its TapeApi.
    core::arch::global_asm!(
        r#"
        .global fos_service_entry
    fos_service_entry:
        mov x0, x20
        blr x19
        bl {exit}
        brk #0
    "#,
        exit = sym super::task_exit,
    );

    unsafe extern "C" {
        fn fos_task_switch(save: *mut u64, to: u64);
        fn fos_service_entry();
    }

    /// Bytes `fos_task_switch` pushes.
    const SWITCH_FRAME: u64 = 176;

    pub unsafe fn switch(save: *mut u64, load: u64) {
        unsafe { fos_task_switch(save, load) }
    }

    pub unsafe fn first_frame(below: u64, entry: u64, api: u64) -> u64 {
        let sp = (below - SWITCH_FRAME) & !15;
        let words = sp as *mut u64;
        unsafe {
            core::ptr::write_bytes(sp as *mut u8, 0, SWITCH_FRAME as usize);
            words.write(entry);
            words.add(1).write(api);
            words.add(11).write(fos_service_entry as *const () as u64);
        }
        sp
    }
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
        ctx::switch(save, load);
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
    host::service_poll();
    let me = unsafe { CURRENT } - 1;
    services()[me].idle = true;
    yield_now();
    services()[me].idle = false;
}

pub extern "C" fn task_exit() -> ! {
    let id = unsafe { CURRENT };
    let service = &mut services()[id - 1];
    for nic in (0..64).filter(|&nic| service.nics >> nic & 1 != 0) {
        host::nic_release(nic);
    }
    crate::audio::release_service();
    crate::println!("Service {} ended.", service.name());
    service.state = State::Dead;
    LIVE.fetch_sub(1, Ordering::Relaxed);
    if count() == 0 {
        host::services_over();
        crate::framebuffer::split_services_over();
    }
    let next = next_runnable(id);
    let mut unused = 0u64;
    unsafe {
        CURRENT = next;
        SLICE_START = timer::micros();
        ctx::switch(&mut unused, saved_rsp(next));
    }
    unreachable!("a finished service was resumed");
}

/// Splits a trailing `&N` or `&N,M,...` off the arguments of `run`: `None` when there is none, `Err` when it is not a list of
/// numbers, `Ok(mask)` otherwise (bit i = 0-based NIC i; `&0` is an empty mask).
pub fn split_suffix(arguments: &str) -> (&str, Option<Result<u64, ()>>) {
    let arguments = arguments.trim();
    match arguments.rsplit_once(char::is_whitespace) {
        Some((before, last)) if last.starts_with('&') => (before.trim(), Some(nic_list(&last[1..]))),
        _ => (arguments, None),
    }
}

/// "1,3" as a NIC mask (1-based numbers; 0 means none and stands alone).
fn nic_list(list: &str) -> Result<u64, ()> {
    let mut mask = 0u64;
    for number in list.split(',') {
        match number.parse::<usize>().map_err(|_| ())? {
            0 => {}
            n if n <= 64 => mask |= 1 << (n - 1),
            _ => return Err(()),
        }
    }
    Ok(mask)
}

/// `split=v` (stacked) in either tape's arguments of `run A | B`; side by side otherwise or for `split=h`.
pub fn split_mode(first: &str, second: &str) -> crate::framebuffer::Split {
    let stacked = first.split_ascii_whitespace().chain(second.split_ascii_whitespace()).any(|word| word == "split=v");
    if stacked { crate::framebuffer::Split::Stacked } else { crate::framebuffer::Split::Sides }
}

/// `rotate=r|l` in either tape's arguments of `run A + B` / `run A | B`: the monitor is mounted turned clockwise or
/// anti-clockwise (see `framebuffer::Rotate`).
pub fn rotate_option(first: &str, second: &str) -> Option<crate::framebuffer::Rotate> {
    first.split_ascii_whitespace().chain(second.split_ascii_whitespace()).find_map(|word| word.strip_prefix("rotate=")).and_then(crate::framebuffer::Rotate::parse)
}

/// The `rotate=` of a single tape's arguments (`run NAME rotate=l`).
pub fn own_rotation(arguments: &str) -> Option<crate::framebuffer::Rotate> {
    rotate_option(arguments, "")
}

/// The `screen=N` of a `run A | B` pair: the physical screen the pair shares (either tape may name it).
pub fn group_screen(first: &str, second: &str) -> Option<usize> {
    first.split_ascii_whitespace().chain(second.split_ascii_whitespace()).find_map(|word| word.strip_prefix("screen=")?.parse().ok())
}

/// `arguments` without any `split=` or `rotate=` word, plus `screen=N` written into `buffer`. A `screen=` of the tape's own
/// stays unless `force` (the halves of a split screen are numbered by the kernel, `framebuffer::api_gfx_info_on`).
/// `None` when it does not fit.
pub fn with_screen<'a>(buffer: &'a mut [u8; 256], arguments: &str, screen: usize, force: bool) -> Option<&'a str> {
    use core::fmt::Write;
    struct Out<'b>(&'b mut [u8], usize);
    impl Write for Out<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let end = self.1 + text.len();
            self.0.get_mut(self.1..end).ok_or(core::fmt::Error)?.copy_from_slice(text.as_bytes());
            self.1 = end;
            Ok(())
        }
    }
    let mut out = Out(&mut buffer[..], 0);
    let mut named = false;
    for word in arguments.split_ascii_whitespace() {
        if word.starts_with("split=") || word.starts_with("rotate=") || (force && word.starts_with("screen=")) {
            continue;
        }
        named |= word.starts_with("screen=");
        write!(out, "{word} ").ok()?;
    }
    if !named {
        write!(out, "screen={screen}").ok()?;
    }
    let length = out.1;
    core::str::from_utf8(&buffer[..length]).ok().map(str::trim)
}

pub fn usage() {
    crate::println!("run: `&N` or `&N,M` names the NICs the background tape owns (from 1 to {}; `&0` for none).", host::nic_count());
}

/// `run NAME ARGS &N`: tape `name` becomes a service owning the NICs in the mask `nics` (bit i = 0-based NIC i; 0 for none). The kernel's
/// `host::load` has the file ready (x86: the staging buffer; aarch64: it reads it).
pub fn start(name: &str, arguments: &str, nics: u64) {
    if in_service() {
        return crate::println!("run: a service cannot start another.");
    }
    let word = |key: &str| arguments.split_ascii_whitespace().find_map(|word| crate::audio::parse_device(word.strip_prefix(key)?));
    // `nic=N,M` is `&N,M`.
    let nics = if nics == 0 { arguments.split_ascii_whitespace().find_map(|word| nic_list(word.strip_prefix("nic=")?).ok()).unwrap_or(0) } else { nics };
    if let Some(nic) = (0..64).find(|&nic| nics >> nic & 1 != 0 && nic >= host::nic_count()) {
        return crate::println!("run: no NIC {} (this machine has {}).", nic + 1, host::nic_count());
    }
    if let Some(nic) = (0..64).find(|&nic| nics >> nic & 1 != 0 && service_owns(nic)) {
        return crate::println!("run: a service already owns NIC {}.", nic + 1);
    }
    if arguments.len() > ARGS_BYTES {
        return crate::println!("run: the arguments of a service take at most {ARGS_BYTES} bytes.");
    }
    let Some(slot) = services().iter().position(|service| service.state == State::Free) else {
        return crate::println!("run: {MAX_SERVICES} services are running already (`ps`, `kill N`).");
    };
    if crate::audio::claim(slot + 1, word("device="), word("outdev=")).is_err() {
        return;
    }
    let Some(entry) = host::load(slot + 1, name) else { return crate::audio::unclaim(slot + 1) };
    let Some(stack) = memory::allocate_contiguous(STACK_BYTES / memory::FRAME, None) else {
        return crate::println!("run: no memory for the service's stack.");
    };
    // The top of the stack holds what the tape reads for as long as it runs: its API table and argument text.
    let args_at = (stack + STACK_BYTES - ARGS_BYTES as u64) & !15;
    let api_at = (args_at - host::API_BYTES as u64) & !15;
    let bytes = arguments.as_bytes();
    let sp = unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), args_at as *mut u8, bytes.len());
        let text = core::str::from_utf8_unchecked(core::slice::from_raw_parts(args_at as *const u8, bytes.len()));
        host::write_api(api_at, text);
        ctx::first_frame(api_at, entry, api_at)
    };
    let service = &mut services()[slot];
    *service = Service::EMPTY;
    let take = name.len().min(16);
    service.name[..take].copy_from_slice(&name.as_bytes()[..take]);
    service.name_length = take;
    service.rsp = sp;
    service.stack = stack;
    service.nics = nics;
    service.state = State::Live;
    LIVE.fetch_add(1, Ordering::Relaxed);
    for nic in (0..64).filter(|&nic| nics >> nic & 1 != 0) {
        host::nic_claim(nic);
    }
    host::services_begin();
    let mut owned = [0u8; 24];
    let owned = nic_text(&mut owned, nics);
    crate::println!("Service {name} started{owned}: `ps` lists services, `kill {}` stops this one.", slot + 1);
    yield_now(); // let it start and print its first lines
}

/// " on NIC 1,2" for the NICs in a mask (empty for none).
fn nic_text(buffer: &mut [u8; 24], nics: u64) -> &str {
    use core::fmt::Write;
    struct Out<'b>(&'b mut [u8], usize);
    impl Write for Out<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let end = self.1 + text.len();
            self.0.get_mut(self.1..end).ok_or(core::fmt::Error)?.copy_from_slice(text.as_bytes());
            self.1 = end;
            Ok(())
        }
    }
    let mut out = Out(&mut buffer[..], 0);
    let mut first = true;
    for nic in (0..64).filter(|&nic| nics >> nic & 1 != 0) {
        write!(out, "{}{}", if first { " on NIC " } else { "," }, nic + 1).ok();
        first = false;
    }
    let length = out.1;
    core::str::from_utf8(&buffer[..length]).unwrap_or("")
}

/// "  audio N out M" for the devices a service holds (nothing when it holds none).
fn claim_text(buffer: &mut [u8; 24], input: usize, output: usize) -> &str {
    use core::fmt::Write;
    struct Out<'b>(&'b mut [u8], usize);
    impl Write for Out<'_> {
        fn write_str(&mut self, text: &str) -> core::fmt::Result {
            let end = self.1 + text.len();
            self.0.get_mut(self.1..end).ok_or(core::fmt::Error)?.copy_from_slice(text.as_bytes());
            self.1 = end;
            Ok(())
        }
    }
    let mut out = Out(&mut buffer[..], 0);
    if input != 0 {
        write!(out, "  audio {input}").ok();
    }
    if output != 0 {
        write!(out, "  out {output}").ok();
    }
    let length = out.1;
    core::str::from_utf8(&buffer[..length]).unwrap_or("")
}

/// `tasks`
pub fn list() {
    if count() == 0 {
        return crate::println!("No services running (`run NAME ARGS &N` starts one on NIC N, `&0` on none).");
    }
    for (index, service) in services().iter().enumerate().filter(|(_, service)| service.state == State::Live) {
        let state = if service.abort { "stopping" } else if service.idle { "idle" } else { "running" };
        let (input, output) = crate::audio::claims_of(index + 1);
        let mut audio = [0u8; 24];
        let audio = claim_text(&mut audio, input, output);
        let mut owned = [0u8; 24];
        let owned = nic_text(&mut owned, service.nics);
        crate::println!("{}  {:<16} {}  {state}{audio}", index + 1, service.name(), owned.strip_prefix(" on ").unwrap_or("no NIC"));
    }
}

/// Flags service `index` (0-based) to stop at its next Escape check.
fn stop(index: usize) {
    let service = &mut services()[index];
    service.abort = true;
    crate::println!("Stopping service {} ({}); it ends the next time it checks for Escape.", index + 1, service.name());
}

fn settle() {
    for _ in 0..4 {
        yield_now();
    }
}

/// `kill N`, `kill NAME` (every service running that tape) or `kill all`
pub fn kill(argument: Option<&str>) {
    let Some(word) = argument else {
        return crate::println!("Usage: kill N | NAME | all   (see `ps`)");
    };
    if let Some(number) = word.parse::<usize>().ok().filter(|n| (1..=MAX_SERVICES).contains(n)) {
        if services()[number - 1].state != State::Live {
            return crate::println!("kill: no service {number}.");
        }
        stop(number - 1);
        return settle();
    }
    let mut found = false;
    for index in 0..MAX_SERVICES {
        let service = &services()[index];
        if service.state == State::Live && !service.abort && (word == "all" || service.name() == word) {
            found = true;
            stop(index);
        }
    }
    if !found {
        return crate::println!("kill: no service \"{word}\".");
    }
    settle();
}
