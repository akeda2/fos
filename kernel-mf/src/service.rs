//! Named services: the `service` command and the `services` file of `settings/`.
//!
//! A line is `NAME [restart] TAPE [ARGS...] &NIC`, the command `run` would take (a service must be a background tape, so
//! it ends in `&N`, `&NAME` or `&+N`). `service start|stop|restart NAME|all` acts on one service or on every one without
//! touching the others, `service` lists them and `service reload` reads the file again. A `start` file that holds the
//! single line `service start all` replaces one `run` line per tape.
//!
//! With `restart` a service that ends without a `service stop` is started again, after 2 s, then 4 s, 8 s ... at most 60 s
//! apart, so a tape that dies at once does not spin. A service that ran for a minute starts the delays over.
//! Everything here runs on the service thread (shell commands and `poll`), so the table lock is never contended.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::{apic, sched};
use crate::sync::SpinLock;

const NAME_MAX: usize = 15;
/// How long `stop` waits for the process to end.
const STOP_WAIT_MS: u64 = 5000;
const CHECK_TICKS: u64 = apic::TICK_HZ / 2;
const MAX_DELAY_S: u64 = 60;
const STABLE_S: u64 = 60;

struct Service {
    name: String,
    tape: String,
    args: String,
    restart: bool,
    /// 0 when the tape is not running.
    pid: u64,
    /// Started and not stopped on purpose: only then is it restarted.
    wanted: bool,
    started_at: u64,
    failures: u32,
    /// Tick of the next start attempt, 0 for none.
    retry_at: u64,
}

static SERVICES: SpinLock<Vec<Service>> = SpinLock::new(Vec::new());
static LOADED: AtomicBool = AtomicBool::new(false);
static LAST_CHECK: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn alive(pid: u64) -> bool {
    let mut found = false;
    sched::for_each_process(|other, _, _, _| found |= other == pid);
    found
}

fn valid_name(name: &str) -> bool {
    (1..=NAME_MAX).contains(&name.len()) && name.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-') && name != "all"
}

/// `NAME [restart] TAPE ARGS... &NIC`
fn parse(line: &str) -> Result<Service, &'static str> {
    let mut words = line.split_whitespace();
    let name = words.next().ok_or("empty line")?;
    if !valid_name(name) {
        return Err("a name is 1-15 letters, digits or `-`, and not `all`");
    }
    let mut rest = line.trim_start()[name.len()..].trim_start();
    let mut restart = false;
    if let Some(after) = rest.strip_prefix("restart").filter(|after| after.starts_with(char::is_whitespace)) {
        restart = true;
        rest = after.trim_start();
    }
    let (tape, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    if tape.is_empty() {
        return Err("no tape named");
    }
    if !args.split_whitespace().last().is_some_and(|last| last.starts_with('&') && last.len() > 1) {
        return Err("a service is a background tape: the line must end in `&N`, `&NAME` or `&+N`");
    }
    Ok(Service { name: name.into(), tape: tape.into(), args: args.trim().into(), restart, pid: 0, wanted: false, started_at: 0, failures: 0, retry_at: 0 })
}

/// Reads `settings/services`. A service that is gone from the file but still runs stays listed (it can be stopped).
fn load() {
    LOADED.store(true, Ordering::Release);
    let Some(text) = crate::config::read("services") else { return };
    let text = String::from(text);
    let mut fresh = Vec::new();
    for line in crate::config::lines(&text) {
        match parse(line) {
            Ok(service) if fresh.iter().any(|other: &Service| other.name == service.name) => println!("services: `{}` is listed twice; the first stands.", service.name),
            Ok(service) => fresh.push(service),
            Err(reason) => println!("services: `{line}`: {reason}."),
        }
    }
    let mut table = SERVICES.lock();
    let old = core::mem::take(&mut *table);
    for mut service in fresh {
        if let Some(previous) = old.iter().find(|previous| previous.name == service.name) {
            (service.pid, service.wanted, service.started_at, service.failures, service.retry_at) =
                (previous.pid, previous.wanted, previous.started_at, previous.failures, previous.retry_at);
        }
        table.push(service);
    }
    for previous in old {
        if previous.pid != 0 && !table.iter().any(|service| service.name == previous.name) {
            table.push(previous);
        }
    }
}

fn ensure_loaded() {
    if !LOADED.load(Ordering::Acquire) {
        load();
    }
}

/// Starts service `index` now; true when the tape runs.
fn start_one(index: usize) -> bool {
    let (name, tape, args) = {
        let table = SERVICES.lock();
        let service = &table[index];
        (service.name.clone(), service.tape.clone(), service.args.clone())
    };
    println!("service {name}: run {tape} {args}");
    crate::shell::run_tape(&tape, &args);
    let pid = crate::shell::LAST_STARTED.load(Ordering::Acquire);
    let mut table = SERVICES.lock();
    let Some(service) = table.iter_mut().find(|service| service.name == name) else { return pid != 0 };
    service.pid = pid;
    service.started_at = apic::ticks();
    pid != 0
}

fn delay_ticks(failures: u32) -> u64 {
    (1u64 << failures.min(6)).min(MAX_DELAY_S) * apic::TICK_HZ
}

/// Kills the process and waits for it to go (the NIC is free when it has).
fn stop_process(pid: u64) -> bool {
    sched::kill(pid);
    let mut waited = 0;
    while alive(pid) {
        if waited >= STOP_WAIT_MS {
            return false;
        }
        sched::yield_now();
        apic::busy_wait_ms(10);
        waited += 10;
    }
    true
}

fn stop_one(index: usize) {
    let (name, pid) = {
        let mut table = SERVICES.lock();
        let service = &mut table[index];
        service.wanted = false;
        service.retry_at = 0;
        (service.name.clone(), core::mem::replace(&mut service.pid, 0))
    };
    if pid == 0 {
        return println!("service {name}: not running.");
    }
    if stop_process(pid) {
        println!("service {name}: stopped (was process {pid}).");
    } else {
        println!("service {name}: process {pid} did not end within {} s.", STOP_WAIT_MS / 1000);
        SERVICES.lock()[index].pid = pid;
    }
}

fn begin(index: usize) {
    {
        let mut table = SERVICES.lock();
        let service = &mut table[index];
        if service.pid != 0 && alive(service.pid) {
            return println!("service {}: already running (process {}).", service.name, service.pid);
        }
        service.wanted = true;
        service.failures = 0;
        service.retry_at = 0;
    }
    if !start_one(index) {
        let mut table = SERVICES.lock();
        let service = &mut table[index];
        if service.restart {
            service.failures += 1;
            service.retry_at = apic::ticks() + delay_ticks(service.failures);
            println!("service {}: did not start; trying again in {} s.", service.name, delay_ticks(service.failures) / apic::TICK_HZ);
        } else {
            service.wanted = false;
        }
    }
}

/// Restarts services whose process ended (called from the service loop, twice a second at most).
pub fn poll() {
    let now = apic::ticks();
    if now.saturating_sub(LAST_CHECK.load(Ordering::Relaxed)) < CHECK_TICKS || !LOADED.load(Ordering::Acquire) {
        return;
    }
    LAST_CHECK.store(now, Ordering::Relaxed);
    let mut due = Vec::new();
    {
        let mut table = SERVICES.lock();
        for (index, service) in table.iter_mut().enumerate() {
            if service.pid != 0 && !alive(service.pid) {
                let ran = now.saturating_sub(service.started_at) / apic::TICK_HZ;
                println!("service {}: process {} ended after {ran} s.", service.name, service.pid);
                service.pid = 0;
                if service.wanted && service.restart {
                    service.failures = if ran >= STABLE_S { 1 } else { service.failures + 1 };
                    service.retry_at = now + delay_ticks(service.failures);
                    println!("service {}: starting again in {} s.", service.name, delay_ticks(service.failures) / apic::TICK_HZ);
                } else {
                    service.wanted = false;
                }
            }
            if service.pid == 0 && service.wanted && service.retry_at != 0 && now >= service.retry_at {
                due.push(index);
            }
        }
    }
    for index in due {
        if !start_one(index) {
            let mut table = SERVICES.lock();
            let service = &mut table[index];
            service.failures += 1;
            service.retry_at = now + delay_ticks(service.failures);
            println!("service {}: did not start; trying again in {} s.", service.name, delay_ticks(service.failures) / apic::TICK_HZ);
        } else {
            SERVICES.lock()[index].retry_at = 0;
        }
    }
}

fn list() {
    let now = apic::ticks();
    let table = SERVICES.lock();
    if table.is_empty() {
        return println!("No services. A line `NAME [restart] TAPE ARGS... &NIC` in `services` defines one (docs/config.md).");
    }
    println!("NAME             STATE                  PID  ADDRESS          COMMAND");
    for service in table.iter() {
        let state = if service.pid != 0 {
            String::from("running")
        } else if service.wanted && service.retry_at != 0 {
            alloc::format!("restart in {} s", service.retry_at.saturating_sub(now) / apic::TICK_HZ)
        } else {
            String::from("stopped")
        };
        let flag = if service.restart { " [restart]" } else { "" };
        let address = crate::tapenet::address_of_owner(service.pid).map_or(String::from("-"), |ip| alloc::format!("{}.{}.{}.{}", ip[0], ip[1], ip[2], ip[3]));
        println!("{:<16} {:<18} {:>6}  {:<15}  {} {}{flag}", service.name, state, service.pid, address, service.tape, service.args);
    }
}

/// The indices a NAME or `all` selects.
fn select(word: &str) -> Option<Vec<usize>> {
    let table = SERVICES.lock();
    if word == "all" {
        return Some((0..table.len()).collect());
    }
    table.iter().position(|service| service.name == word).map(|index| alloc::vec![index])
}

pub fn command<'a>(mut words: impl Iterator<Item = &'a str> + Clone) {
    ensure_loaded();
    let Some(verb) = words.next() else { return list() };
    if verb == "reload" {
        load();
        return list();
    }
    let action: fn(usize) = match verb {
        "start" => begin,
        "stop" => stop_one,
        "restart" => |index| {
            stop_one(index);
            begin(index);
        },
        _ => return println!("usage: service [start|stop|restart NAME|all | reload]"),
    };
    let Some(target) = words.next() else { return println!("usage: service {verb} NAME|all") };
    match select(target) {
        None => println!("service: no service `{target}` (`service` lists them)."),
        Some(indices) if indices.is_empty() => println!("service: none defined (see `services` in docs/config.md)."),
        Some(indices) => {
            let mut indices = indices;
            // `stop all` goes backwards, so the last started goes first
            if verb == "stop" {
                indices.reverse();
            }
            for index in indices {
                action(index);
            }
        }
    }
}
