//! The x86 shell commands that are not in the shared tables (docs/shell.md): a table like the Pi's in `shell.rs`, so `help`
//! lists them and `help NAME` answers for them. The handlers that need `term.rs` internals (the NIC chip tests, `power`)
//! stay there and are called from here.

use fosfor_shell::Command;

/// `nic park|unpark|phy|ts|csum|vlan|mcast|regs`: the x86-only, chip-specific forms. False for any other `nic` word,
/// which the shared `nic` command (`nicshell.rs`) answers.
pub fn nic(rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    match words.next() {
        Some(word @ ("park" | "unpark")) => crate::term::nic_park(word == "park", words.next()),
        Some("phy") => crate::term::nic_phy(words.next()),
        Some("ts") => crate::term::nic_timestamp_test(words.next(), words.next(), words.next()),
        Some("csum") => crate::term::nic_checksum_test(words.next()),
        Some("vlan") => crate::term::nic_vlan_test(words.next(), words.next(), words.next()),
        Some("mcast") => crate::term::nic_multicast_test(words.next(), words.next(), words.next()),
        Some("regs") => crate::term::nic_regs(words.next().and_then(|word| word.parse::<usize>().ok())),
        _ => return false,
    }
    true
}

/// `usb try|abort-test`: hidden diagnostics. False for any other `usb` word, which `usbshell.rs` answers.
pub fn usb(rest: &str) -> bool {
    let mut words = rest.split_whitespace();
    match words.next() {
        Some("abort-test") => match crate::xhci::controller(0) {
            // See `Controller::abort_self_test`.
            Some(controller) => crate::println!("xHCI command abort self-test: {}.", if controller.abort_self_test() { "passed (the ring carries on)" } else { "FAILED" }),
            None => crate::println!("xHCI command abort self-test: no xHCI controller."),
        },
        Some("try") => match words.next().and_then(|word| word.parse::<u8>().ok()) {
            Some(port) => crate::usb_hid::try_port(port, words.next()),
            None => crate::println!("Usage: usb try PORT [hot|warm]   (a diagnostic: reset the port by hand, then enumerate it once)"),
        },
        _ => return false,
    }
    true
}

pub const COMMANDS: &[Command] = &[
    Command::new("tape", "tape load NAME|net | run [ARGS] | save NAME | status", "loads a tape into memory, runs it, saves it, or shows what is loaded.", |_, rest| crate::tape::command(rest)),
    Command::new("install", "install diskN [LABEL] [bare] [force]   (then: install diskN confirm)", "copies the boot disk (ESP, kernel, files, folders) onto another disk.", |_, rest| crate::install::command(rest.split_whitespace())),
    Command::new("bootcode", "bootcode [diskN] | bootcode update [diskN]   (then: ... confirm)", "shows or replaces the BIOS boot code and the UEFI loader from files on disk1.", |_, rest| crate::bootcode::command(rest.split_whitespace())),
    Command::new("videomodes", "videomodes", "the screen modes the BIOS video BIOS offered, and what the monitor told.", |_, _| crate::term::videomodes_command()),
    Command::new("hda", "hda", "describes the Intel HD Audio controllers and their codecs.", |_, _| crate::hda::describe()),
    Command::new("gpu", "gpu", "reads (never writes) what the firmware left running on an Intel display engine.", |_, _| crate::gpu::command()),
    Command::new("midi", "midi | midi monitor | midi send [#N] HEX HEX [HEX]", "USB-MIDI devices: list them, watch what they send, send a message.", |_, rest| crate::midishell::command(&mut rest.split_whitespace())),
    Command::new("touch", "touch | touch monitor", "USB touch panels: list them, or watch finger events.", |_, rest| crate::usb_pointer::command(&mut rest.split_whitespace())),
    Command {
        name: "power",
        aliases: &["cpufreq", "temp"],
        usage: "power [N] | power hwp [performance|balanced|power]",
        summary: "how much the CPU slept in hlt; HWP frequency scaling; `cpufreq` and `temp` (temperature, speed and power now) are plain `power`.",
        run: |_, rest| crate::term::power(rest),
    },
];
