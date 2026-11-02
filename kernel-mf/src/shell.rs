//! A minimal server shell: a line editor fed byte by byte from COM1 and from the network terminal, with a handful of
//! inspection commands. Output goes through `println!`, which reaches every console (serial and, while a session is open,
//! the network). Runs on the boot CPU from the service loop.

use crate::{apic, block, net, percpu, pmm, power, sched, slots};
use kernel_core::netdump::Mac;
use kernel_core::{nic, pci};

const PROMPT: &str = "fos> ";
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
                prompt();
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
        }
        "version" => println!("Fosfor OS server kernel (mf), slice 7."),
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
        "echo" => println!("{}", line[command.len()..].trim_start()),
        "reboot" => {
            println!("Restarting.");
            slots::mark_clean_stop();
            power::reset();
        }
        _ => println!("Unknown command `{command}`; try `help`."),
    }
}
