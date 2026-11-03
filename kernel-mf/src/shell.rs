//! A minimal server shell: a line editor fed byte by byte from COM1 and from the network terminal, with a handful of
//! inspection commands. Output goes through `println!`, which reaches every console (serial and, while a session is open,
//! the network). Runs on the boot CPU from the service loop.

use crate::{apic, block, clock, net, percpu, pmm, power, sched, slots};
use kernel_core::netdump::Mac;
use kernel_core::{nic, pci};

const PROMPT: &str = "fos> ";
const ESCAPE: u8 = 0x1b;

/// Input state of the foreground tape: while a tape runs, typed bytes go to it (`read_key`) instead of the line editor;
/// Escape kills it, as on the embedded kernel.
struct Keys {
    pid: u64,
    buffer: [u8; 64],
    head: usize,
    count: usize,
}
static KEYS: crate::sync::SpinLock<Keys> = crate::sync::SpinLock::new(Keys { pid: 0, buffer: [0; 64], head: 0, count: 0 });

/// Makes `pid` the tape that receives typed bytes. Gives up at once if it already ended.
pub fn set_foreground(pid: u64) {
    if crate::process::outcomes().iter().any(|outcome| outcome.pid == pid) {
        return;
    }
    let mut keys = KEYS.lock();
    keys.pid = pid;
    keys.head = 0;
    keys.count = 0;
}

/// The tape `pid` ended: the shell takes the keyboard back (and says so with a fresh prompt).
pub fn end_foreground(pid: u64) {
    let mut keys = KEYS.lock();
    if keys.pid == pid {
        keys.pid = 0;
        drop(keys);
        prompt();
    }
}

/// `read_key` for process `pid`: the next typed byte, if it is the foreground tape.
pub fn take_key(pid: u64) -> Option<u8> {
    let mut keys = KEYS.lock();
    if keys.pid != pid || keys.count == 0 {
        return None;
    }
    let byte = keys.buffer[keys.head];
    keys.head = (keys.head + 1) % keys.buffer.len();
    keys.count -= 1;
    Some(byte)
}

/// Routes a byte to the foreground tape; false when there is none.
fn feed_foreground(byte: u8) -> bool {
    let mut keys = KEYS.lock();
    let pid = keys.pid;
    if pid == 0 {
        return false;
    }
    if byte == ESCAPE {
        drop(keys);
        sched::kill(pid);
        return true;
    }
    if keys.count < keys.buffer.len() {
        let tail = (keys.head + keys.count) % keys.buffer.len();
        keys.buffer[tail] = byte;
        keys.count += 1;
    }
    true
}
static mut LINE: [u8; 128] = [0; 128];
static mut LENGTH: usize = 0;

pub fn prompt() {
    print!("{PROMPT}");
}

/// Forgets a half-typed line (a new terminal session starts clean).
pub fn reset() {
    unsafe { LENGTH = 0 };
}

/// One input byte: printable characters are echoed, Backspace edits, Enter runs the line.
pub fn feed(byte: u8) {
    if feed_foreground(byte) {
        return;
    }
    unsafe {
        let line = &mut *core::ptr::addr_of_mut!(LINE);
        match byte {
            b'\r' | b'\n' => {
                println!();
                let length = LENGTH;
                LENGTH = 0;
                if let Ok(text) = core::str::from_utf8(&line[..length]) {
                    run(text.trim());
                }
                if KEYS.lock().pid == 0 {
                    prompt();
                }
            }
            0x08 | 0x7f => {
                if LENGTH > 0 {
                    LENGTH -= 1;
                    print!("\x08 \x08");
                }
            }
            0x20..=0x7e if LENGTH < line.len() => {
                line[LENGTH] = byte;
                LENGTH += 1;
                print!("{}", byte as char);
            }
            _ => {}
        }
    }
}

/// Runs one command line (the `start` file's lines come here).
pub fn run(line: &str) {
    let mut words = line.split_whitespace();
    let Some(command) = words.next() else { return };
    if command != "format" {
        crate::formatshell::cancel(); // a format needs its confirmation as the very next command
    }
    if command != "install" {
        crate::install::cancel();
    }
    if command != "bootcode" {
        crate::bootcode::cancel();
    }
    // Commands that touch a volume directly keep tape file calls off the disk meanwhile.
    let _disk = matches!(command, "format" | "label" | "fsck" | "grow" | "install" | "bootcode").then(crate::tapefs::lock);
    match command {
        "help" => match line.trim_start()[command.len()..].trim() {
            "" => {
                println!("help version cpus mem pci disks nic uptime slots threads echo TEXT reboot audio usb date nic [peers|discover] tapesync gpio @HOST ...");
                println!("ls cat write rm cp mv cmp mkdir (`help NAME` shows one; files are NAME, diskN/NAME, @LABEL/NAME)");
                println!("keymap kup serial  recv NAME  io sleep clear memmap bootinfo cpuinfo  start dmesg sector mount umount label fsck grow sha256 format install bootcode (disk commands ask first)");
                println!("tape NAME [ARGS]  run a tape (a ring 3 process)   NAME [ARGS]  the same   kill PID");
            }
            topic => {
                let mut host = crate::shellhost::MfHost;
                if !fosfor_shell::execute(&mut host, &[fosfor_shell::files::COMMANDS], line) {
                    println!("help: {topic} has no help screen here (try the tape's own `{topic} help`).");
                }
            }
        },
        "version" => println!("Fosfor OS server kernel (mf), slice 8."),
        "cpus" => println!("{} CPU(s); boot CPU has taken {} timer ticks.", percpu::count(), percpu::ticks_of(0)),
        "mem" => {
            let stats = pmm::stats();
            println!("{} MiB free of {} MiB managed.", stats.free * pmm::FRAME >> 20, stats.managed * pmm::FRAME >> 20);
        }
        "pci" => {
            for device in pci::devices() {
                println!("{:02x}:{:02x}.{} {:04x}:{:04x} {}", device.bus, device.device, device.function, device.vendor, device.id, pci::class_name(device.class, device.subclass));
            }
        }
        "disks" => {
            for index in 0..block::count() {
                if let Some(disk) = block::get(index) {
                    println!("disk{}: {} MiB", index + 1, disk.sectors >> 11);
                }
            }
        }
        "nic" if matches!(words.clone().next(), Some("peers")) => crate::netclient::print_peers(),
        "nic" if matches!(words.clone().next(), Some("discover")) => crate::netclient::nic_discover(),
        "tapesync" => crate::netclient::tapesync(words),
        "gpio" => crate::netclient::gpio(line.trim_start().strip_prefix("gpio").unwrap_or("")),
        "nic" => {
            for index in 0..nic::count() {
                if let Some(mac) = net::mac(index) {
                    println!("nic{}: {} link {}", index + 1, Mac(&mac), if net::link_up(index) { "up" } else { "down" });
                }
            }
        }
        "threads" => {
            sched::for_each(|id, name, state| println!("{id:3} {name:<16} {state:?}"));
            println!("{} context switches.", sched::switches());
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
        "uptime" => println!("{} s", apic::ticks() / apic::TICK_HZ),
        "slots" => slots::report(),
        "audio" => crate::audioshell::command(words),
        "usb" => crate::usbshell::command(words),
        "tape" => {
            let Some(name) = words.next() else { return println!("usage: tape NAME [ARGS]") };
            let args = line.trim_start()[command.len()..].trim_start()[name.len()..].trim_start();
            run_tape(name, args);
        }
        "kill" => match words.next().and_then(|word| word.parse::<u64>().ok()) {
            Some(pid) => sched::kill(pid),
            None => println!("usage: kill PID (the number `tape` printed)"),
        },
        "echo" => println!("{}", line[command.len()..].trim_start()),
        // Test hooks for the unattended-operation machinery (`debug crash`: a #UD in the kernel, `debug hang`: the service thread spins).
        "debug" => match words.next() {
            // The terminal's Escape key is a break, so a test cannot type ESC: this prints every colour through the console.
            Some("colors") => {
                for code in (30..=37).chain(90..=97) {
                    print!("\x1b[{code}mcolor {code}\x1b[0m ");
                }
                println!("\x1b[41;1m bold on red \x1b[0m");
            }
            Some("slots") | Some("status") => slots::report(),
            Some("crash") => unsafe { core::arch::asm!("ud2") },
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
            _ => println!("usage: debug slots|crash|hang|deadhang"),
        },
        "start" => crate::config::start(run),
        "keymap" => crate::sysshell::keymap(&line.trim_start()[command.len()..]),
        "kup" => crate::sysshell::kup(&line.trim_start()[command.len()..]),
        "recv" => crate::sysshell::recv(&mut crate::shellhost::MfHost, &line.trim_start()[command.len()..]),
        "dmesg" => crate::sysshell::dmesg(),
        "clear" => crate::sysshell::clear(),
        "sleep" => crate::sysshell::sleep(&line.trim_start()[command.len()..]),
        "io" => crate::sysshell::io(&line.trim_start()[command.len()..]),
        "memmap" => crate::sysshell::memmap(),
        "bootinfo" => crate::sysshell::bootinfo(),
        "cpuinfo" => crate::sysshell::cpuinfo(),
        "sector" => crate::sysshell::sector(&line.trim_start()[command.len()..]),
        "mount" => crate::sysshell::mount(&line.trim_start()[command.len()..]),
        "umount" => crate::sysshell::umount(&line.trim_start()[command.len()..]),
        "format" => crate::formatshell::command(&line.trim_start()[command.len()..]),
        "label" => crate::sysshell::label(&line.trim_start()[command.len()..]),
        "fsck" => crate::sysshell::fsck(&line.trim_start()[command.len()..]),
        "grow" => crate::sysshell::grow(&line.trim_start()[command.len()..]),
        "sha256" => crate::sysshell::sha256(&mut crate::shellhost::MfHost, &line.trim_start()[command.len()..]),
        "install" => crate::install::command(words),
        "bootcode" => crate::bootcode::command(words),
        "reboot" => {
            println!("Restarting.");
            slots::mark_clean_stop();
            power::reset();
        }
        _ => {
            let mut host = crate::shellhost::MfHost;
            if !fosfor_shell::execute(&mut host, &[fosfor_shell::files::COMMANDS], line) && !fosfor_shell::external(&mut host, line) {
                println!("Unknown command `{command}`; try `help`.");
            }
        }
    }
}

/// Loads the ELF tape `name` from the boot volume (or whichever volume holds it) and starts it as a process.
pub fn run_tape(name: &str, args: &str) {
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
    match crate::process::spawn_elf(name, &image, args.as_bytes()) {
        Ok(pid) => {
            println!("tape: `{name}` is process {pid}; Escape stops it.");
            set_foreground(pid);
        }
        Err(error) => println!("tape: `{name}` cannot start: {error:?}."),
    }
}
