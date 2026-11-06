//! `top [SECONDS] [once]`: a live view of the threads and processes: CPU use over the last interval, the CPU each runs on
//! (or last ran on) and the hardware it holds (NICs, audio, the pixel screen, file streams).
//!
//! The shell runs on the service thread, which must keep polling NICs and the serial port, so `top` does not block: `start`
//! arms it, the service loop calls `poll` every pass and `poll` redraws once the interval is over, and `key` (called first by
//! `shell::feed`) ends it on q, Escape or Ctrl-C and swallows other keys while it runs. `once` prints one plain frame
//! (no screen control) and stops, for scripts and tests.
//!
//! CPU use is sampled: each CPU's timer tick charges the thread it finds running (10 ms), which is exact enough over a second
//! and costs nothing in the scheduler's hot path. A CPU's load leaves out its idle thread and the `service` thread, which polls
//! by design and would always read 100 %.

use crate::{apic, percpu, pmm, process, sched, tapeaudio, tapefs, tapenet};
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use kernel_core::nic::MAX_NICS;

struct Top {
    interval: u64,
    next: u64,
    once: bool,
    drawn: bool,
    wall: u64,
    switches: u64,
    threads: Vec<(usize, u64)>,
    busy: Vec<u64>,
}

/// One per shell session.
static TOP: [crate::sync::SpinLock<Option<Top>>; crate::session::MAX] = [const { crate::sync::SpinLock::new(None) }; crate::session::MAX];

struct Row {
    id: usize,
    pid: u64,
    name: String,
    state: sched::State,
    running_on: Option<usize>,
    last_cpu: usize,
    pinned: Option<usize>,
    percent: u64,
    ticks: u64,
}

fn busy_now() -> Vec<u64> {
    (0..percpu::count()).map(sched::busy_ticks).collect()
}

fn threads_now() -> Vec<(usize, u64)> {
    let mut all = Vec::new();
    sched::snapshot(|info| all.push((info.id, info.ticks)));
    all
}

/// `top [SECONDS] [once]`.
pub fn start(rest: &str) {
    let mut interval = 2;
    let mut once = false;
    for word in rest.split_whitespace() {
        match (word, word.parse::<u64>()) {
            ("once", _) => once = true,
            (_, Ok(seconds)) if (1..=60).contains(&seconds) => interval = seconds,
            _ => return println!("Usage: top [SECONDS (1-60)] [once]   (q or Escape ends it)"),
        }
    }
    let now = apic::ticks();
    *TOP[crate::session::current_or_console()].lock() = Some(Top { interval: interval * apic::TICK_HZ, next: now + interval * apic::TICK_HZ, once, drawn: false, wall: now, switches: sched::switches(), threads: threads_now(), busy: busy_now() });
    if !once {
        print!("\x1b[?25l\x1b[2J\x1b[H(collecting for {interval} s...)");
    }
}

/// Whether `top` is running (the shell then leaves the prompt to it).
pub fn active() -> bool {
    TOP[crate::session::current_or_console()].lock().is_some()
}

/// Called every pass of the service loop: redraws when the interval is over.
pub fn poll() {
    for id in 0..crate::session::MAX {
        if TOP[id].lock().is_some() {
            crate::session::with(id, || poll_session(id));
        }
    }
}

fn poll_session(id: usize) {
    let mut guard = TOP[id].lock();
    let Some(top) = guard.as_mut() else { return };
    let now = apic::ticks();
    if now < top.next {
        return;
    }
    let wall = (now - top.wall).max(1);
    let mut frame = String::new();
    render(&mut frame, top, wall, !top.once);
    top.wall = now;
    top.next = now + top.interval;
    top.switches = sched::switches();
    top.threads = threads_now();
    top.busy = busy_now();
    top.drawn = true;
    let finished = top.once;
    if finished {
        *guard = None;
    }
    drop(guard);
    print!("{frame}");
    if finished {
        crate::shell::prompt();
    }
}

/// A typed byte while `top` runs: q, Escape or Ctrl-C end it. True when `top` took the byte.
pub fn key(byte: u8) -> bool {
    let mut guard = TOP[crate::session::current_or_console()].lock();
    if guard.is_none() {
        return false;
    }
    if matches!(byte, b'q' | b'Q' | 0x1b | 0x03) {
        let once = guard.take().is_some_and(|top| top.once);
        drop(guard);
        if !once {
            print!("\x1b[?25h\x1b[2J\x1b[H");
        }
        crate::shell::prompt();
    }
    true
}

fn render(out: &mut String, top: &Top, wall: u64, screen: bool) {
    let before = |id: usize| top.threads.iter().find(|entry| entry.0 == id).map_or(0, |entry| entry.1);
    let mut rows: Vec<Row> = Vec::new();
    let mut running_name: Vec<Option<String>> = (0..percpu::count()).map(|_| None).collect();
    sched::snapshot(|info| {
        if info.idle {
            return;
        }
        let percent = (info.ticks.saturating_sub(before(info.id)) * 100 / wall).min(100);
        if let Some(cpu) = info.running_on.filter(|&cpu| cpu < running_name.len() && info.name() != "service") {
            running_name[cpu] = Some(String::from(info.name()));
        }
        rows.push(Row { id: info.id, pid: info.pid, name: String::from(info.name()), state: info.state, running_on: info.running_on, last_cpu: info.last_cpu, pinned: info.pinned, percent, ticks: info.ticks });
    });
    rows.sort_by(|a, b| b.percent.cmp(&a.percent).then(a.id.cmp(&b.id)));

    let uptime = apic::ticks() / apic::TICK_HZ;
    let memory = pmm::stats();
    let (free, total) = (memory.free * pmm::FRAME >> 20, memory.managed * pmm::FRAME >> 20);
    let switches = sched::switches();
    let line_end = if screen { "\x1b[K\r\n" } else { "\r\n" };
    if screen {
        out.push_str("\x1b[H");
    }
    let _ = write!(out, "top - up {}:{:02}:{:02}, {} CPU(s), {free} of {total} MiB free, {} context switches/s{line_end}", uptime / 3600, uptime / 60 % 60, uptime % 60, percpu::count(), (switches - top.switches) * apic::TICK_HZ / wall);
    let now_busy = busy_now();
    for cpu in 0..percpu::count() {
        let load = (now_busy[cpu].saturating_sub(top.busy.get(cpu).copied().unwrap_or(0)) * 100 / wall).min(100);
        let bar = (load as usize + 5) / 10;
        let _ = write!(out, "CPU{cpu:<2} [{}{}] {load:>3}%  {}{line_end}", "#".repeat(bar), " ".repeat(10 - bar), running_name[cpu].as_deref().unwrap_or("-"));
    }
    let _ = write!(out, "{line_end} PID TID NAME           STATE     CPU PIN  %CPU     TIME  HARDWARE{line_end}");
    for row in rows.iter().take(20) {
        let cpu = match row.running_on {
            Some(cpu) => alloc::format!("{cpu}"),
            None => alloc::format!("({})", row.last_cpu),
        };
        let pin = row.pinned.map_or(String::from("-"), |cpu| alloc::format!("{cpu}"));
        let mut hardware = String::new();
        if row.pid != 0 {
            held(row.pid, &mut hardware);
        }
        let state = alloc::format!("{:?}", row.state);
        let _ = write!(out, "{:4} {:3} {:<14.14} {state:<9} {cpu:>3} {pin:>3} {:>4}% {:>5}.{}s  {hardware}{line_end}", row.pid, row.id, row.name, row.percent, row.ticks / apic::TICK_HZ, row.ticks % apic::TICK_HZ / 10);
    }
    let owner_name = |pid: u64| rows.iter().find(|row| row.pid == pid).map_or(alloc::format!("pid {pid}"), |row| alloc::format!("{} ({pid})", row.name));
    let _ = write!(out, "{line_end}Hardware:{line_end}");
    for index in (0..MAX_NICS).filter(|&index| tapenet::exists(index)) {
        let owner = tapenet::owner(index);
        let who = if owner == 0 { String::from("free (shell)") } else { owner_name(owner) };
        let _ = write!(out, "  nic{}    {who}{line_end}", index + 1);
    }
    for (label, owner) in [("audio", tapeaudio::owner()), ("screen", process::screen_owner()), ("streams", tapefs::stream_owner())] {
        let who = if owner == 0 { String::from("free") } else { owner_name(owner) };
        let _ = write!(out, "  {label:<8}{who}{line_end}");
    }
    if screen {
        out.push_str("\x1b[J  q or Escape ends\r\n");
    }
}

/// The hardware process `pid` holds, as words.
fn held(pid: u64, out: &mut String) {
    for index in (0..MAX_NICS).filter(|&index| tapenet::exists(index) && tapenet::owner(index) == pid) {
        let _ = write!(out, "nic{} ", index + 1);
    }
    if tapeaudio::owner() == pid {
        out.push_str("audio ");
    }
    if process::screen_owner() == pid {
        out.push_str("screen ");
    }
    if tapefs::stream_owner() == pid {
        out.push_str("fs-stream ");
    }
}
