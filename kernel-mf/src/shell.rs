//! A minimal server shell: a line editor fed byte by byte from COM1 and from the network terminal, with a handful of
//! inspection commands. Output goes through `println!`, which reaches every console (serial and, while a session is open,
//! the network). Runs on the boot CPU from the service loop.

use crate::{time, block, clock, net, percpu, pmm, power, sched, slots};
use kernel_core::netdump::Mac;
use alloc::vec::Vec;
use kernel_core::{nic, pci};

const PROMPT: &str = "fos> ";
const ESCAPE: u8 = 0x1b;

/// Input state of the foreground tape: while a tape runs, typed bytes go to it (`read_key`) instead of the line editor;
/// Escape asks it to stop (`request_stop`), as on the embedded kernel.
struct Keys {
    pid: u64,
    buffer: [u8; 64],
    head: usize,
    count: usize,
    /// Tick at which an Escape arrived that may start a cursor-key sequence (`ESC [ D`); 0 when none is pending.
    escape_tick: u64,
}
/// One per session (`session.rs`).
static KEYS: [crate::sync::SpinLock<Keys>; crate::session::MAX] = [const { crate::sync::SpinLock::new(Keys { pid: 0, buffer: [0; 64], head: 0, count: 0, escape_tick: 0 }) }; crate::session::MAX];

/// How long a tape asked to stop (Escape, its session closing) has to end by itself before it is killed: long enough to
/// close a file and drain a writer thread over a slow USB device, short enough that a tape that never looks is not missed.
const STOP_GRACE_TICKS: u64 = 3 * time::TICK_HZ;

/// Tapes asked to stop: `(pid, tick by which it must have ended)`; 0 is a free entry.
static STOPS: crate::sync::SpinLock<[(u64, u64); 8]> = crate::sync::SpinLock::new([(0, 0); 8]);

/// Asks process `pid` to end: its `read_key` answers `KEY_ABORT` and `should_abort` true from now on, which is what a tape on
/// the embedded kernel sees on Escape, so it can close its files and end itself. Asked a second time (a second Escape), or
/// when it has not ended `STOP_GRACE_TICKS` later (`poll_escape`), it is killed outright.
pub fn request_stop(pid: u64) {
    let mut stops = STOPS.lock();
    if stops.iter().any(|&(asked, _)| asked == pid) {
        stops.iter_mut().filter(|entry| entry.0 == pid).for_each(|entry| *entry = (0, 0));
        drop(stops);
        println!("Process {pid}: asked twice; killed.");
        return sched::kill(pid);
    }
    match stops.iter_mut().find(|entry| entry.0 == 0) {
        Some(entry) => *entry = (pid, time::ticks() + STOP_GRACE_TICKS),
        None => {
            drop(stops);
            sched::kill(pid);
        }
    }
}

/// Whether process `pid` has been asked to stop (`request_stop`).
pub fn stop_requested(pid: u64) -> bool {
    pid != 0 && STOPS.lock().iter().any(|&(asked, _)| asked == pid)
}

/// Kills the tapes that were asked to stop and are still running at their deadline; forgets the ones that ended.
fn poll_stops() {
    let now = time::ticks();
    let mut stops = STOPS.lock();
    for entry in stops.iter_mut().filter(|entry| entry.0 != 0) {
        let (pid, deadline) = *entry;
        let mut running = false;
        sched::for_each_process(|p, _, _, _| running |= p == pid);
        if !running {
            *entry = (0, 0);
        } else if now >= deadline {
            *entry = (0, 0);
            println!("Process {pid} did not end within {} s of being asked; killed.", STOP_GRACE_TICKS / time::TICK_HZ);
            sched::kill(pid);
        }
    }
}

/// The foreground tape of session `id` (0 when none).
pub fn foreground(id: usize) -> u64 {
    KEYS[id].lock().pid
}

/// The session leaves: its foreground tape keeps running but no longer gets keys.
pub fn detach(id: usize) {
    let mut keys = KEYS[id].lock();
    keys.pid = 0;
    keys.head = 0;
    keys.count = 0;
    keys.escape_tick = 0;
}

/// How long an Escape waits for the `[` of a sequence; a terminal sends the bytes back to back, a person never types that fast.
const ESCAPE_WAIT_TICKS: u64 = 3;

/// Makes `pid` the tape that receives the current session's typed bytes. Gives up at once if it already ended.
pub fn set_foreground(pid: u64) {
    if crate::process::outcomes().iter().any(|outcome| outcome.pid == pid) {
        return;
    }
    let mut keys = KEYS[crate::session::current_or_console()].lock();
    keys.pid = pid;
    keys.head = 0;
    keys.count = 0;
    keys.escape_tick = 0;
}

/// The tape `pid` ended: the shell takes the keyboard back (and says so with a fresh prompt).
pub fn end_foreground(pid: u64) {
    for id in 0..crate::session::MAX {
        let mut keys = KEYS[id].lock();
        if keys.pid == pid {
            keys.pid = 0;
            drop(keys);
            crate::session::with(id, prompt);
            return;
        }
    }
}

/// `read_key` for process `pid`: the next typed byte, if it is the foreground tape.
pub fn take_key(pid: u64) -> Option<u8> {
    let id = (0..crate::session::MAX).find(|&id| foreground(id) == pid)?;
    let mut keys = KEYS[id].lock();
    if keys.pid != pid || keys.count == 0 {
        return None;
    }
    let byte = keys.buffer[keys.head];
    keys.head = (keys.head + 1) % keys.buffer.len();
    keys.count -= 1;
    Some(byte)
}

fn push_key(keys: &mut Keys, byte: u8) {
    if keys.count < keys.buffer.len() {
        let tail = (keys.head + keys.count) % keys.buffer.len();
        keys.buffer[tail] = byte;
        keys.count += 1;
    }
}

/// Routes a byte to the foreground tape; false when there is none. A lone Escape asks the tape to stop, but Escape followed at once
/// by `[` or `O` is a cursor key and goes to the tape as typed (the embedded kernel does the same).
fn feed_foreground(id: usize, byte: u8) -> bool {
    let mut keys = KEYS[id].lock();
    let pid = keys.pid;
    if pid == 0 {
        return false;
    }
    if keys.escape_tick != 0 {
        keys.escape_tick = 0;
        if byte == b'[' || byte == b'O' {
            push_key(&mut keys, ESCAPE);
            push_key(&mut keys, byte);
            return true;
        }
        drop(keys);
        request_stop(pid);
        return true;
    }
    if byte == ESCAPE {
        keys.escape_tick = time::ticks().max(1);
        return true;
    }
    push_key(&mut keys, byte);
    true
}

/// A lone Escape that nothing followed in time asks the foreground tape to stop, and a tape that was asked and did not end in
/// time is killed; called from the service loop.
pub fn poll_escape() {
    poll_stops();
    for id in 0..crate::session::MAX {
        let mut keys = KEYS[id].lock();
        if keys.escape_tick != 0 && time::ticks().saturating_sub(keys.escape_tick) >= ESCAPE_WAIT_TICKS {
            keys.escape_tick = 0;
            let pid = keys.pid;
            drop(keys);
            if pid != 0 {
                request_stop(pid);
            }
        }
    }
}
pub fn prompt() {
    let folder = crate::shellhost::working_folder();
    if folder.is_empty() {
        print!("{PROMPT}");
    } else {
        print!("fos {folder}> ");
    }
}

/// Commands `run` handles itself, outside the shared tables, for Tab completion.
const BUILTINS: &[&str] = &[
    "version", "cpus", "mem", "pci", "disks", "nic", "vnic", "service", "settings", "tapesync", "gpio", "top", "threads", "ps", "date", "uptime", "slots", "audio", "i2s", "i2c", "touch", "midi", #[cfg(target_arch = "aarch64")] "cpufreq", #[cfg(target_arch = "aarch64")] "temp", "usb", "tape", "run", "kill",
    "echo", "debug", "sysrq", "trace", "start", "keymap", "kup", "recv", "dmesg", "clear", "sleep", "io", "memmap", "bootinfo", "cpuinfo", "shrink", "sector", "readspeed", "writespeed", "mkimage", "mount", "umount",
    "format", "label", "fsck", "grow", "install", "bootcode", "reboot", "reset", "shutdown", "halt", "ext4", "md", "sha256", "cd", "pwd", "exit",
];

/// Installs the line editor's Tab completion (the editor itself, with history, is `kernel_core::lineedit`).
pub fn init() {
    kernel_core::lineedit::set_completer(|line, add| {
        let mut host = crate::shellhost::MfHost;
        let names: Vec<&str> = BUILTINS.iter().copied().chain(crate::private::TOPICS.iter().map(|topic| topic.name)).collect();
        fosfor_shell::complete::complete(&mut host, &[fosfor_shell::files::COMMANDS], &names, line, add);
    });
}

/// Forgets session `id`'s half-typed line (a new terminal session starts clean; the history stays).
pub fn reset(id: usize) {
    kernel_core::lineedit::select(id);
    kernel_core::lineedit::reset();
}

/// One byte typed on the console.
pub fn feed(byte: u8) {
    feed_session(0, byte);
}

/// A key from a local keyboard (PS/2 or USB). Its decoders give cursor keys and å ä ö private codes >= 0x80, which the line
/// editor understands; a running tape gets what a terminal would send instead (ESC [ A for Up, UTF-8), as on the embedded
/// kernel (`term.rs` `tape_service`). Serial and `term net` bytes already are that, so they keep using `feed`.
pub fn feed_key(key: u8) {
    if foreground(0) != 0 {
        if let Some(sequence) = fosfor_abi::keymap::ansi_sequence(key) {
            feed(0x1b);
            feed(b'[');
            return sequence.iter().for_each(|&byte| feed(byte));
        }
        if let Some(sequence) = fosfor_abi::keymap::utf8_sequence(key) {
            return sequence.iter().for_each(|&byte| feed(byte));
        }
    }
    feed(key);
}

/// One input byte for session `id`: the line editor handles it (cursor keys, history, Tab); Enter runs the line.
pub fn feed_session(id: usize, byte: u8) {
    crate::session::with(id, || feed_in(id, byte));
}

fn feed_in(id: usize, byte: u8) {
    if crate::top::key(byte) || crate::audioshell::key(byte) {
        return;
    }
    if feed_foreground(id, byte) {
        return;
    }
    kernel_core::lineedit::select(id);
    // Ctrl-D on an empty line is end of input: it ends a login session (ssh) like `exit`. A network terminal's client ends itself on it.
    if byte == 0x04 && kernel_core::lineedit::is_empty() && crate::net::exit_session(id) {
        println!("logout");
        return;
    }
    let mut line = [0u8; kernel_core::lineedit::LINE_MAX];
    if let Some(length) = kernel_core::lineedit::feed(byte, &mut line) {
        // A copy: running a command may reset the editor or start a session that feeds it again.
        if let Ok(text) = core::str::from_utf8(&line[..length]) {
            run(text.trim());
        }
        if foreground(id) == 0 && !crate::top::active() {
            prompt();
        }
    }
}

/// Runs one line: the scripting language when the line uses it (`crate::script::wants`), else one command.
pub fn run(line: &str) {
    if crate::script::wants(line) {
        crate::script::interactive(line);
    } else {
        run_command(line);
    }
}

/// Runs one plain command (no language): what the interpreter itself calls for every command it runs.
pub fn run_command(line: &str) {
    let mut words = line.split_whitespace();
    let Some(command) = words.next() else { return };
    if !matches!(command, "format" | "shrink" | "ext4") {
        crate::formatshell::cancel(); // a format needs its confirmation as the very next command
    }
    if command != "md" {
        crate::mdvol::cancel();
    }
    if command != "install" {
        crate::install::cancel();
    }
    if command != "bootcode" {
        crate::bootcode::cancel();
    }
    // Commands that touch a volume directly keep tape file calls off the disk meanwhile.
    let _disk = matches!(command, "format" | "label" | "fsck" | "grow" | "shrink" | "install" | "bootcode").then(crate::tapefs::lock);
    match command {
        "help" => match line.trim_start()[command.len()..].trim() {
            "" => crate::helptext::overview(fosfor_shell::files::COMMANDS),
            topic => {
                let mut host = crate::shellhost::MfHost;
                if let Some(builtin) = crate::helptext::find(topic) {
                    crate::helptext::show(builtin);
                } else if !fosfor_shell::execute(&mut host, &[fosfor_shell::files::COMMANDS], line) {
                    println!("help: {topic} has no help screen (and is no command).");
                }
            }
        },
        "cd" => {
            let target = line.trim_start()[command.len()..].trim();
            if crate::shellhost::change_folder(target).is_err() {
                println!("cd: {target}: no such folder.");
            }
        }
        "exit" | "logout" => {
            if !crate::net::exit_session(crate::session::current_or_console()) {
                println!("exit: this is not a login session (a network terminal ends with its client, the console never).");
            }
        }
        "pwd" => {
            let folder = crate::shellhost::working_folder();
            println!("{}", if folder.is_empty() { "/ (the boot volume)" } else { &folder });
        }
        "version" => println!("Fosfor OS server kernel (mf), slice 8{}.", if cfg!(target_arch = "aarch64") { ", aarch64" } else { "" }),
        "cpus" => println!("{} CPU(s); boot CPU has taken {} timer ticks.", percpu::count(), percpu::ticks_of(0)),
        "mem" => {
            let stats = pmm::stats();
            println!("{} MiB free of {} MiB managed.", stats.free * pmm::FRAME >> 20, stats.managed * pmm::FRAME >> 20);
        }
        "pci" if words.clone().next().is_some() => pci_inspect(words),
        "pci" => {
            for device in pci::devices() {
                println!("{:02x}:{:02x}.{} {:04x}:{:04x} {}", device.bus, device.device, device.function, device.vendor, device.id, pci::class_name(device.class, device.subclass));
            }
        }
        "disks" => {
            for index in 0..block::count() {
                if let Some(disk) = block::get(index) {
                    println!("disk{}: {} MiB", index + 1, disk.sectors >> 11);
                    kernel_core::ext4vol::describe(index, &mut crate::sysshell::Console);
                }
            }
        }
        "nic" if matches!(words.clone().next(), Some("peers")) => crate::netclient::print_peers(),
        "nic" if matches!(words.clone().next(), Some("discover")) => crate::netclient::nic_discover(),
        "tapesync" => crate::netclient::tapesync(words),
        "gpio" => crate::netclient::gpio(line.trim_start().strip_prefix("gpio").unwrap_or("")),
        "nic" => match words.next() {
            None => net::print_list(),
            Some("regs") => match words.next().and_then(|word| word.parse().ok()) {
                Some(number) => net::print_registers(number),
                None => println!("usage: nic regs N"),
            },
            Some("mtu") => net::nic_mtu(words.next(), words.next(), words.next(), words.next()),
            Some(_) => println!("usage: nic [regs N | mtu [N [SIZE]]]"),
        },
        "vnic" => crate::vnic::command(words),
        "service" => crate::service::command(words),
        "settings" => match words.next() {
            Some("migrate") => migrate_settings(),
            _ => println!("usage: settings migrate   (moves the root's *.cfg and smbusers into settings/)"),
        },
        "top" => crate::top::start(&line.trim_start()[command.len()..]),
        "threads" => {
            // Print after the scheduler lock is released: println! asks the scheduler which session is running.
            let mut rows = alloc::vec::Vec::new();
            sched::for_each(|id, name, state| rows.push((id, alloc::string::String::from(name), state)));
            for (id, name, state) in rows {
                println!("{id:3} {name:<16} {state:?}");
            }
            println!("{} context switches.", sched::switches());
        }
        "ps" => {
            println!("PID  THREAD NAME             STATE");
            let mut rows = alloc::vec::Vec::new();
            sched::for_each_process(|pid, id, name, state| rows.push((pid, id, alloc::string::String::from(name), state)));
            for (pid, id, name, state) in rows {
                println!("{pid:<4} {id:<6} {name:<16} {state:?}");
            }
            println!("`kill PID` stops one; `threads` lists the kernel threads too.");
        }
        "date" => match words.next() {
            None => match clock::now() {
                Some(time) => println!("{} (local time, from the RTC)", clock::Display(time)),
                None => println!("The clock is not set. `date set YYYY-MM-DD HH:MM`."),
            },
            Some("set") => {
                let parsed = (|| {
                    let (date, time) = (words.next()?, words.next()?);
                    let mut parts = date.split('-');
                    let (year, month, day) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
                    let mut parts = time.split(':');
                    let (hour, minute) = (parts.next()?.parse().ok()?, parts.next()?.parse().ok()?);
                    let second = parts.next().map_or(Some(0), |word| word.parse().ok())?;
                    Some(fat::Time { year, month, day, hour, minute, second })
                })();
                match parsed {
                    None => println!("Usage: date set YYYY-MM-DD HH:MM[:SS]   (local time)"),
                    Some(time) => match clock::set(&time) {
                        Ok(()) => println!("The clock is {}.", clock::Display(time)),
                        Err(reason) => println!("date: {reason}"),
                    },
                }
            }
            Some(_) => println!("Usage: date | date set YYYY-MM-DD HH:MM[:SS]"),
        },
        "uptime" => println!("{} s", time::ticks() / time::TICK_HZ),
        "slots" => slots::report(),
        "audio" => crate::audioshell::command(words),
        "touch" => crate::usbshell::touch(words),
        "midi" => crate::usbshell::midi(words),
        #[cfg(target_arch = "aarch64")]
        "cpufreq" | "temp" if !crate::hal::is_pi4() => println!("cpufreq: needs the Raspberry Pi 4's firmware mailbox."),
        #[cfg(target_arch = "aarch64")]
        "cpufreq" | "temp" => crate::pipower::command(line.trim_start().split_once(char::is_whitespace).map_or("", |(_, rest)| rest)),
        #[cfg(target_arch = "aarch64")]
        "i2s" => crate::i2s_pi::command(line.trim_start().strip_prefix("i2s").unwrap_or("").trim()),
        #[cfg(target_arch = "aarch64")]
        "i2c" => crate::i2c::command(line.trim_start().strip_prefix("i2c").unwrap_or("").trim()),
        #[cfg(not(target_arch = "aarch64"))]
        "i2s" | "i2c" => println!("{command}: only on a Raspberry Pi 4."),
        "usb" => crate::usbshell::command(words),
        "tape" | "run" => {
            // `run` is the verb; `tape NAME` and `tape run NAME` (the embedded kernels' older spelling) stay accepted.
            let mut rest = line.trim_start()[command.len()..].trim_start();
            if command == "tape" {
                if let Some(after) = rest.strip_prefix("run").filter(|after| after.is_empty() || after.starts_with(char::is_whitespace)) {
                    rest = after.trim_start();
                }
            }
            let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if name.is_empty() {
                return println!("usage: run NAME [ARGS] [&N]");
            }
            let args = args.trim_start();
            run_tape(name, args);
        }
        "kill" => match words.next().and_then(|word| word.parse::<u64>().ok()) {
            Some(pid) => sched::kill(pid),
            None => println!("usage: kill PID (the number `tape` printed)"),
        },
        "echo" => println!("{}", line[command.len()..].trim_start()),
        // Test hooks for the unattended-operation machinery (`debug crash`: a #UD in the kernel, `debug hang`: the service thread spins).
        "debug" => match words.next() {
            // These take the machine down on purpose (rack once lost its crash record to a reset): they need `now` to be typed.
            Some(word @ ("crash" | "hang" | "deadhang" | "deadlock")) if words.next() != Some("now") => println!("debug {word} stops the machine on purpose; type `debug {word} now` to do it."),
            // The terminal's Escape key is a break, so a test cannot type ESC: this prints every colour through the console.
            Some("colors") => {
                for code in (30..=37).chain(90..=97) {
                    print!("\x1b[{code}mcolor {code}\x1b[0m ");
                }
                println!("\x1b[41;1m bold on red \x1b[0m");
            }
            Some("slots") | Some("status") => slots::report(),
            Some("revert") => {
                let _disk = crate::tapefs::lock();
                let mut outcome = Err("the slot metadata cannot be read or written");
                let updated = slots::update_metadata(|m| outcome = kernel_core::slot_report::revert(m));
                match (updated, outcome) {
                    (Some(_), Ok(slot)) => println!("Slot {slot} is the active slot now; `reboot` starts its kernel."),
                    (_, Err(why)) => println!("debug revert: {why}."),
                    _ => println!("debug revert: the slot metadata could not be written."),
                }
            }
            Some("crash") => crate::arch::crash_now(),
            Some("hang") => {
                // The shell runs on the service thread: spinning here with interrupts on stops its petting while other threads run.
                loop {
                    core::hint::spin_loop();
                }
            }
            // Interrupts off on this CPU too (what a `term net` shell hang looks like): only the hardware watchdog can end it.
            Some("deadhang") => {
                crate::cpu::interrupts_disable();
                loop {
                    core::hint::spin_loop();
                }
            }
            // A CPU waits for a lock it holds itself: the spinlock deadlock detector must report both places and reset.
            Some("deadlock") => {
                static LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());
                let _held = LOCK.lock();
                let _again = LOCK.lock();
            }
            _ => println!("usage: debug slots|revert|crash now|hang now|deadhang now|deadlock now"),
        },
        "sysrq" => crate::sysrq::command(),
        "trace" => kernel_core::trace::command(&line.trim_start()[command.len()..]),
        "start" => crate::config::start(run),
        "keymap" => crate::sysshell::keymap(&line.trim_start()[command.len()..]),
        "kup" => crate::sysshell::kup(&line.trim_start()[command.len()..]),
        "recv" => crate::sysshell::recv(&mut crate::shellhost::MfHost, &line.trim_start()[command.len()..]),
        "dmesg" => crate::sysshell::dmesg(),
        "clear" => crate::sysshell::clear(),
        "sleep" => crate::sysshell::sleep(&line.trim_start()[command.len()..]),
        "io" => crate::arch::cpuinfo::io(&line.trim_start()[command.len()..]),
        "memmap" => crate::sysshell::memmap(),
        "bootinfo" => crate::sysshell::bootinfo(),
        "cpuinfo" => crate::arch::cpuinfo::cpuinfo(),
        "shrink" => crate::formatshell::shrink(&line.trim_start()[command.len()..]),
        "ext4" => crate::sysshell::ext4(&line.trim_start()[command.len()..]),
        "sector" => crate::sysshell::sector(&line.trim_start()[command.len()..]),
        "readspeed" => crate::sysshell::readspeed(&line.trim_start()[command.len()..]),
        "writespeed" => crate::sysshell::writespeed(&line.trim_start()[command.len()..]),
        "mkimage" => crate::mkimage::command(&line.trim_start()[command.len()..]),
        "mount" => crate::sysshell::mount(&line.trim_start()[command.len()..]),
        "umount" => crate::sysshell::umount(&line.trim_start()[command.len()..]),
        "format" => crate::formatshell::command(&line.trim_start()[command.len()..]),
        "label" => crate::sysshell::label(&line.trim_start()[command.len()..]),
        "fsck" => crate::sysshell::fsck(&line.trim_start()[command.len()..]),
        "grow" => crate::sysshell::grow(&line.trim_start()[command.len()..]),
        "sha256" => crate::sysshell::sha256(&mut crate::shellhost::MfHost, &line.trim_start()[command.len()..]),
        "install" => crate::install::command(words),
        "bootcode" => crate::bootcode::command(words),
        "md" => crate::mdvol::command(words),
        "aoe" => crate::aoetarget::command(words),
        "shutdown" | "halt" => {
            println!("Powering off.");
            crate::mdvol::stop_all();
            slots::mark_clean_stop();
            power::shutdown();
            println!("shutdown: the machine did not power off (no ACPI _S5_ or the chipset ignored it).");
        }
        "reboot" | "reset" => {
            println!("Restarting.");
            crate::mdvol::stop_all();
            slots::mark_clean_stop();
            power::reset();
        }
        _ if crate::private::command(command, words.clone()) => {}
        _ => {
            let mut host = crate::shellhost::MfHost;
            if !fosfor_shell::execute(&mut host, &[fosfor_shell::files::COMMANDS], line) && !fosfor_shell::external(&mut host, line) {
                println!("Unknown command `{command}`; try `help`.");
            }
        }
    }
}

/// Loads the ELF tape `name` from the boot volume (or whichever volume holds it) and starts it as a process.
/// Splits a trailing `&N` or `&N,M` off the arguments of `run`: the 0-based NIC mask (`&0`: none) of a background tape.
/// `Err` when the `&` word is not a list of NIC numbers.
fn split_background(args: &str) -> (&str, Option<Result<u64, ()>>) {
    let args = args.trim();
    let Some((before, last)) = Some(args.rsplit_once(char::is_whitespace).unwrap_or(("", args))).filter(|(_, last)| last.starts_with('&')) else {
        return (args, None);
    };
    let mut mask = 0u64;
    for number in last[1..].split(',') {
        if let Some(index) = crate::vnic::lookup(number) {
            mask |= 1 << index;
            continue;
        }
        match number.parse::<usize>() {
            Ok(0) => {}
            Ok(n) if n <= 64 => mask |= 1 << (n - 1),
            _ => return (before.trim(), Some(Err(()))),
        }
    }
    (before.trim(), Some(Ok(mask)))
}

/// `run NAME ARGS`: the tape becomes the foreground process (Escape stops it). With `&N` it is a background tape (a service):
/// it owns NIC N, keeps running while the shell takes commands, and ends with `kill PID`.
/// `settings migrate`: machine-specific files that lie in the root move into `settings/`, where tapes and `service` look first.
fn migrate_settings() {
    let mut names = alloc::vec::Vec::new();
    {
        let _disk = crate::tapefs::lock();
        let mut info = fosfor_abi::tape::FileInfo { length: 0, flags: 0, name_length: 0, name: [0; 64] };
        let mut index = 0;
        while kernel_core::tape_fs::list(b"", index, &mut info) == 1 {
            index += 1;
            if info.flags & fosfor_abi::tape::FILE_DIRECTORY != 0 {
                continue;
            }
            let name = alloc::string::String::from_utf8_lossy(&info.name[..info.name_length as usize]).into_owned();
            if name.ends_with(".cfg") || name == "smbusers" || name == "services" {
                names.push(name);
            }
        }
    }
    for name in &names {
        run(&alloc::format!("mv {name} settings/"));
    }
    if names.is_empty() {
        println!("settings: nothing to move (the root has no *.cfg, smbusers or services).");
    }
}

/// The pid the last `run_tape` started, 0 when it did not start anything (`service` reads it).
pub static LAST_STARTED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

pub fn run_tape(name: &str, args: &str) {
    // `NAME help` and `NAME --help` print the tape's usage (its `.fos_help` section) without running it.
    if matches!(args.trim(), "help" | "--help") && fosfor_shell::Host::tape_help(&mut crate::shellhost::MfHost, name) {
        return;
    }
    LAST_STARTED.store(0, core::sync::atomic::Ordering::Release);
    // `rotate=r|l|flip|off` turns what this tape draws, as the `rotate` setting does; the word is not passed on to the tape.
    let mut text = [0u8; 256];
    let mut args = args;
    let mut turn = None;
    if args.split_whitespace().any(|word| word.starts_with("rotate=")) {
        let mut length = 0;
        for word in args.split_whitespace() {
            if let Some(mode) = word.strip_prefix("rotate=") {
                match crate::fb::parse_run_turn(mode) {
                    Some(parsed) => turn = Some(parsed),
                    None => return println!("run: `{word}` is not rotate=r|l|flip|off."),
                }
                continue;
            }
            if length + word.len() + 1 > text.len() {
                return println!("run: the arguments of a tape take at most {} bytes.", text.len() - 1);
            }
            if length != 0 {
                text[length] = b' ';
                length += 1;
            }
            text[length..length + word.len()].copy_from_slice(word.as_bytes());
            length += word.len();
        }
        args = core::str::from_utf8(&text[..length]).unwrap_or("");
    }
    // `&+N`: a virtual interface on physical NIC N made for this tape alone and removed when it ends.
    let mut fresh = None;
    let mut background = None;
    if let Some((before, last)) = Some(args.trim()).map(|text| text.rsplit_once(char::is_whitespace).unwrap_or(("", text))).filter(|(_, last)| last.starts_with("&+")) {
        match last[2..].parse::<usize>().map_err(|_| "`&+N` takes the physical NIC number, from 1").and_then(|parent| crate::vnic::add_ephemeral(name, parent)) {
            Ok(interface) => {
                background = Some(Ok(1u64 << interface.index()));
                fresh = Some(interface);
                args = before;
            }
            Err(reason) => return println!("run: {reason}."),
        }
    }
    let (args, background) = match background {
        Some(background) => (args.trim(), Some(background)),
        None => split_background(args),
    };
    let nics = match background {
        Some(Err(())) => return println!("run: `&N` or `&N,M` names the NICs the background tape owns (`&0` for none)."),
        Some(Ok(mask)) => {
            if let Some(nic) = (0..64).find(|&nic| mask >> nic & 1 != 0 && !crate::tapenet::exists(nic)) {
                return println!("run: no NIC {}.", nic + 1);
            }
            if let Some(nic) = (0..64).find(|&nic| mask >> nic & 1 != 0 && crate::tapenet::owner(nic) != 0) {
                return println!("run: a process already owns NIC {}.", nic + 1);
            }
            Some(mask)
        }
        None => None,
    };
    let disk = crate::tapefs::lock();
    // A bare name not in the root is looked for in `bin/` (a v4 volume keeps its tapes there).
    let mut path = name.as_bytes();
    let mut found = kernel_core::volumes::volume_with_file(path);
    if found.is_none() {
        if let Some(bin) = kernel_core::tape_fs::bin_path(path) {
            found = kernel_core::volumes::volume_with_file(bin);
            path = bin;
        }
    }
    let Some(volume) = found else {
        return println!("tape: no file `{name}`.");
    };
    let length = match volume.find(path) {
        Ok(Some(entry)) if !entry.is_dir => entry.length as usize,
        _ => return println!("tape: cannot find `{name}`."),
    };
    let mut image = alloc::vec![0u8; length];
    if !matches!(volume.read(path, &mut image), Ok(Some(_))) {
        return println!("tape: cannot read `{name}`.");
    }
    drop(disk);
    crate::process::START_NICS.store(nics.unwrap_or(0), core::sync::atomic::Ordering::Release);
    *crate::process::START_ROTATE.lock() = turn;
    let started = crate::process::spawn_elf(name, &image, args.as_bytes());
    crate::process::START_NICS.store(0, core::sync::atomic::Ordering::Release);
    *crate::process::START_ROTATE.lock() = None;
    if let (Some(interface), Ok(_)) = (fresh.take(), &started) {
        interface.keep();
    }
    if let Ok(pid) = &started {
        LAST_STARTED.store(*pid, core::sync::atomic::Ordering::Release);
    }
    match started {
        Ok(pid) => match nics {
            Some(_) => {
                println!("tape: `{name}` runs in the background as process {pid} (`kill {pid}` stops it).");
            }
            None => {
                println!("tape: `{name}` is process {pid}; Escape stops it.");
                set_foreground(pid);
            }
        },
        Err(error) => println!("tape: `{name}` cannot start: {error:?}."),
    }
}

/// `pci BB:DD.F [probe|diff]`: the config space and BARs of one device; `probe` also switches on memory decoding and reads
/// the first 256 bytes of every memory BAR (reads only, nothing is written to the device), for finding out what an
/// undocumented card is.
fn pci_inspect<'a>(mut words: impl Iterator<Item = &'a str>) {
    let address = words.next().unwrap_or("");
    let parse = || -> Option<(u8, u8, u8)> {
        let (bus, rest) = address.split_once(':')?;
        let (device, function) = rest.split_once('.')?;
        Some((u8::from_str_radix(bus, 16).ok()?, u8::from_str_radix(device, 16).ok()?, function.parse().ok()?))
    };
    let Some((bus, slot, function)) = parse() else { return println!("usage: pci [BB:DD.F [probe]]") };
    let Some(device) = pci::devices().find(|d| (d.bus, d.device, d.function) == (bus, slot, function)) else {
        return println!("no such device");
    };
    println!("{:04x}:{:04x} class {:02x}{:02x}{:02x} rev {:02x}", device.vendor, device.id, device.class, device.subclass, device.interface, device.revision);
    println!("subsystem {:04x}:{:04x}, command/status {:08x}, interrupt line {}", pci::read32(bus, slot, function, 0x2e) & 0xffff, pci::read32(bus, slot, function, 0x2c) >> 16, device.read32(4), device.interrupt_line());
    // All 256 bytes of the standard space: capabilities such as PCI Express (link control, ASPM) often sit above 0x80.
    for row in 0..16u8 {
        let mut line = alloc::format!("{:02x}:", row * 16);
        for column in 0..4 {
            line += &alloc::format!(" {:08x}", device.read32(row * 16 + column * 4));
        }
        println!("{}", line);
    }
    let mode = words.next();
    let probe = mode == Some("probe");
    if mode == Some("diff") {
        return pci_diff(&device);
    }
    if probe {
        device.write32(4, device.read32(4) & 0xffff | 2);
    }
    for index in 0..6u8 {
        if let Some(port) = device.io_bar(index) {
            println!("BAR{} I/O port {:#x}", index, port);
        } else if let Some((base, size)) = device.memory_bar(index) {
            println!("BAR{} memory {:#x}, {} bytes", index, base, size);
            if probe {
                crate::vm::map_device(base, size.min(0x100));
                let registers = crate::vm::direct(base) as *const u32;
                for row in 0..16 {
                    let mut line = alloc::format!("  {:03x}:", row * 16);
                    for column in 0..4 {
                        line += &alloc::format!(" {:08x}", unsafe { registers.add(row * 4 + column).read_volatile() });
                    }
                    println!("{}", line);
                }
            }
        }
    }
}

/// Reads the first 4 KiB of every memory BAR of `device` eight times, a short busy-wait apart, and prints the dwords
/// that changed (first value, last value, number of distinct values): live counters and status bits show up, constants
/// and RAM do not. Reads only; memory decoding is switched on for it.
fn pci_diff(device: &pci::Device) {
    device.write32(4, device.read32(4) & 0xffff | 2);
    for index in 0..6u8 {
        let Some((base, size)) = device.memory_bar(index) else { continue };
        let length = size.min(4096) as usize;
        crate::vm::map_device(base, length as u64);
        let registers = crate::vm::direct(base) as *const u32;
        let mut samples: Vec<Vec<u32>> = Vec::new();
        for _ in 0..8 {
            samples.push((0..length / 4).map(|i| unsafe { registers.add(i).read_volatile() }).collect());
            for _ in 0..2_000_000 {
                core::hint::spin_loop();
            }
        }
        let mut changed = 0;
        for i in 0..length / 4 {
            let mut distinct: Vec<u32> = samples.iter().map(|sample| sample[i]).collect();
            distinct.sort_unstable();
            distinct.dedup();
            if distinct.len() > 1 {
                changed += 1;
                if changed <= 24 {
                    println!("BAR{} {:03x}: {:08x} -> {:08x}, {} values", index, i * 4, samples[0][i], samples[7][i], distinct.len());
                }
            }
        }
        println!("BAR{}: {} of {} dwords changed", index, changed, length / 4);
    }
}
