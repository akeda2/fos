//! The kernel trace ring: the last `CAPACITY` events (what happened, in which context, when), for the hangs that leave
//! nothing on the screen. Recording is a few stores, so the calls stay in place; categories select what is kept. The
//! `trace` command prints the ring, and the SysRq frame (`docs/design.md` "Debug tools") its tail while a tape runs.
//!
//! An event is a static name and two numbers whose meaning the name gives ("key-in" byte and ring head, "lb-start" loop
//! and ends). Interrupt handlers and other CPUs may record too: the slot is taken with an atomic add, and each slot carries
//! the sequence number of the event in it, written last, so a reader skips a slot that is being rewritten instead of
//! printing half of two events. On the server kernel an event names its CPU and thread (`fos_hal` `trace_origin`).

use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

const CAPACITY: usize = 1024;

/// What an event is about; `trace on|off NAME` selects them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum Category {
    /// Context switches: thousands a second while services run, so off by default.
    Sched = 0,
    /// Keys into and out of a tape's input ring.
    Input = 1,
    /// Tapes and services starting and ending.
    Tape = 2,
    /// Audio and loopback ends.
    Audio = 3,
    /// Frames the kernel handles itself (SysRq).
    Net = 4,
}

pub const CATEGORIES: [(Category, &str); 5] =
    [(Category::Sched, "sched"), (Category::Input, "input"), (Category::Tape, "tape"), (Category::Audio, "audio"), (Category::Net, "net")];

#[derive(Clone, Copy)]
pub struct Event {
    pub micros: u64,
    pub name: &'static str,
    pub a: u64,
    pub b: u64,
    /// The context (embedded kernels) or the CPU (server kernel).
    pub context: u8,
    /// The thread, on the server kernel.
    pub thread: Option<u32>,
    pub category: Category,
}

const EMPTY: Event = Event { micros: 0, name: "", a: 0, b: 0, context: 0, thread: None, category: Category::Tape };

static mut RING: [Event; CAPACITY] = [EMPTY; CAPACITY];
/// Per slot: 1 + the sequence number of the event written there, 0 while it is being written.
static STAMPS: [AtomicUsize; CAPACITY] = [const { AtomicUsize::new(0) }; CAPACITY];
/// Events ever recorded; the newest is at `(NEXT - 1) % CAPACITY`.
static NEXT: AtomicUsize = AtomicUsize::new(0);
static ENABLED: AtomicU32 = AtomicU32::new(!(1 << Category::Sched as u32));

pub fn enabled(category: Category) -> bool {
    ENABLED.load(Ordering::Relaxed) >> category as u32 & 1 != 0
}

pub fn set(category: Category, on: bool) {
    let bit = 1 << category as u32;
    if on {
        ENABLED.fetch_or(bit, Ordering::Relaxed);
    } else {
        ENABLED.fetch_and(!bit, Ordering::Relaxed);
    }
}

/// Records an event if its category is on.
pub fn event(category: Category, name: &'static str, a: u64, b: u64) {
    if !enabled(category) {
        return;
    }
    let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
    let slot = sequence % CAPACITY;
    let (context, thread) = crate::tasks::trace_origin();
    let event = Event { micros: crate::timer::micros(), name, a, b, context: context as u8, thread, category };
    STAMPS[slot].store(0, Ordering::Release);
    unsafe { core::ptr::write_volatile(&raw mut (*(&raw mut RING))[slot], event) };
    STAMPS[slot].store(sequence + 1, Ordering::Release);
}

pub fn clear() {
    NEXT.store(0, Ordering::Relaxed);
    STAMPS.iter().for_each(|stamp| stamp.store(0, Ordering::Relaxed));
}

/// The last `count` events, oldest first.
pub fn recent(count: usize, mut each: impl FnMut(&Event)) {
    let total = NEXT.load(Ordering::Relaxed);
    let count = count.min(total).min(CAPACITY);
    for sequence in total - count..total {
        let slot = sequence % CAPACITY;
        if STAMPS[slot].load(Ordering::Acquire) != sequence + 1 {
            continue;
        }
        let event = unsafe { core::ptr::read_volatile(&raw const (*(&raw const RING))[slot]) };
        if STAMPS[slot].load(Ordering::Acquire) == sequence + 1 {
            each(&event);
        }
    }
}

/// How many events were recorded since boot (or `trace clear`).
pub fn total() -> usize {
    NEXT.load(Ordering::Relaxed)
}

fn category_name(category: Category) -> &'static str {
    CATEGORIES.iter().find(|(c, _)| *c == category).map_or("?", |(_, name)| name)
}

/// One event as a line of at most 79 columns (without the line end): seconds since boot, context (or CPU and thread),
/// category, name, the two numbers.
pub struct Line<'a>(pub &'a Event);

impl core::fmt::Display for Line<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let event = self.0;
        let (seconds, micros, category) = (event.micros / 1_000_000, event.micros % 1_000_000, category_name(event.category));
        match event.thread {
            Some(thread) => write!(f, "{seconds:>5}.{micros:06} {}/t{thread:<3} {category:<5} {:<12} {:#x} {:#x}", event.context, event.name, event.a, event.b),
            None => write!(f, "{seconds:>5}.{micros:06} c{} {category:<5} {:<12} {:#x} {:#x}", event.context, event.name, event.a, event.b),
        }
    }
}

pub fn print_event(event: &Event) {
    crate::println!("{}", Line(event));
}

pub fn print_recent(count: usize) {
    recent(count, print_event);
}

/// `trace [N|all]`, `trace on|off CATEGORY...|all`, `trace clear`.
pub fn command(arguments: &str) {
    let mut words = arguments.split_ascii_whitespace();
    match words.next() {
        None => print_recent(40),
        Some("all") => print_recent(CAPACITY),
        Some("clear") => {
            clear();
            crate::println!("trace: cleared.");
        }
        Some(word @ ("on" | "off")) => {
            let on = word == "on";
            let mut any = false;
            for name in words {
                any = true;
                if name == "all" {
                    CATEGORIES.iter().for_each(|(category, _)| set(*category, on));
                } else if let Some((category, _)) = CATEGORIES.iter().find(|(_, known)| *known == name) {
                    set(*category, on);
                } else {
                    crate::println!("trace: no category {name} (sched input tape audio net all).");
                }
            }
            if !any {
                crate::println!("usage: trace on|off CATEGORY... (sched input tape audio net all)");
            }
            status();
        }
        Some(word) => match word.parse::<usize>() {
            Ok(count) => print_recent(count),
            Err(_) => usage(),
        },
    }
}

pub fn status() {
    crate::print(format_args!("trace: {} event(s) recorded; on:", total()));
    for (category, name) in CATEGORIES {
        if enabled(category) {
            crate::print(format_args!(" {name}"));
        }
    }
    crate::println!();
}

pub fn usage() {
    crate::println!("usage: trace [N|all]           the last N (40) kernel events");
    crate::println!("       trace on|off CATEGORY   sched input tape audio net all");
    crate::println!("       trace clear");
}
