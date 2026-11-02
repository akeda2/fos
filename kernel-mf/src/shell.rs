//! A minimal server shell: a line editor fed byte by byte from COM1 and from the network terminal, with a handful of
//! inspection commands. Output goes through `println!`, which reaches every console (serial and, while a session is open,
//! the network). Runs on the boot CPU from the service loop.

use crate::{apic, block, net, percpu, pmm, power, sched, slots};
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

fn run(line: &str) {
    let mut words = line.split_whitespace();
    let Some(command) = words.next() else { return };
    match command {
        "help" => {
            println!("help version cpus mem pci disks nic uptime slots threads echo TEXT reboot");
            println!("tape NAME [ARGS]  run a tape (a ring 3 process)   kill PID");
        }
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
        "uptime" => println!("{} s", apic::ticks() / apic::TICK_HZ),
        "slots" => match slots::read_metadata() {
            Some(m) => println!("booted slot {}, active {}, pending {:?}, crash count {}", slots::boot_slot(), m.active_slot, m.pending_slot, m.crash_count),
            None => println!("slot metadata unreadable"),
        },
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
        "reboot" => {
            println!("Restarting.");
            slots::mark_clean_stop();
            power::reset();
        }
        _ => println!("Unknown command `{command}`; try `help`."),
    }
}

/// Loads the ELF tape `name` from the boot volume (or whichever volume holds it) and starts it as a process.
fn run_tape(name: &str, args: &str) {
    let disk = crate::tapefs::LOCK.lock();
    let Some(volume) = kernel_core::volumes::volume_with_file(name.as_bytes()) else {
        return println!("tape: no file `{name}`.");
    };
    let length = match volume.find(name.as_bytes()) {
        Ok(Some(entry)) if !entry.is_dir => entry.length as usize,
        _ => return println!("tape: cannot find `{name}`."),
    };
    let mut image = alloc::vec![0u8; length];
    if !matches!(volume.read(name.as_bytes(), &mut image), Ok(Some(_))) {
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
