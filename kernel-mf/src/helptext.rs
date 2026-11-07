//! `help` and `help NAME` for the shell commands `shell.rs` handles itself. The shared file commands (`ls cat ...`) bring
//! their own help (`fosfor_shell::files::COMMANDS`); a tape's help is the `.fos_help` section of its ELF.

pub struct Topic {
    pub name: &'static str,
    /// What `help` lists the command under.
    pub group: &'static str,
    pub usage: &'static str,
    pub summary: &'static str,
}

const fn t(name: &'static str, group: &'static str, usage: &'static str, summary: &'static str) -> Topic {
    Topic { name, group, usage, summary }
}

pub const GROUPS: &[&str] = &["Files", "System", "Network", "Disks", "Tapes and services", "Machine", "Debug"];

pub const TOPICS: &[Topic] = &[
    t("cd", "Files", "cd [FOLDER|..|/]", "Change the working folder; `cd /` returns to the volume root."),
    t("exit", "System", "exit", "End a login session (ssh); also `logout`. Not for the console or `term net`."),
    t("pwd", "Files", "pwd", "Show the working folder."),
    t("sha256", "Files", "sha256 [diskN/]NAME...", "SHA-256 of each file."),
    t("echo", "Files", "echo TEXT", "Print TEXT (useful in scripts, e.g. `echo hi` in `start`)."),
    t("version", "System", "version", "Which kernel this is."),
    t("cpus", "System", "cpus", "Number of CPUs and the boot CPU's timer ticks."),
    t("cpuinfo", "System", "cpuinfo", "CPU vendor, model and feature flags."),
    t("mem", "System", "mem", "Free and managed RAM."),
    t("memmap", "System", "memmap", "The firmware memory map the kernel was given."),
    t("bootinfo", "System", "bootinfo", "What the boot loader passed to the kernel."),
    t("pci", "System", "pci", "List the PCI devices."),
    t("uptime", "System", "uptime", "Seconds since boot."),
    t("date", "System", "date | date set YYYY-MM-DD HH:MM[:SS]", "Show or set the clock (local time, kept in the RTC)."),
    t("dmesg", "System", "dmesg", "The kernel's boot and event log."),
    t("slots", "System", "slots", "Kernel slot state (A/B, pending, last result)."),
    t("keymap", "System", "keymap [us|sv]", "Show or set the keyboard layout."),
    t("clear", "System", "clear", "Clear the screen."),
    t("sleep", "System", "sleep SECONDS", "Wait (for scripts)."),
    t("io", "System", "io r8|r16|r32 PORT | io w8|w16|w32 PORT VALUE", "Read or write an x86 I/O port (decimal or 0x hex)."),
    t("top", "System", "top [SECONDS (1-60)] [once]", "Per-CPU and per-process load; q or Escape ends it."),
    t("threads", "System", "threads", "List the kernel threads and the context-switch count."),
    t("ps", "System", "ps", "List the processes (tapes)."),
    t("nic", "Network", "nic [regs N]", "List the network interfaces; `regs N` dumps the chip registers and ring state of NIC N."),
    t("vnic", "Network", "vnic | vnic add NAME on N [mac=...] | vnic del NAME", "Virtual NICs on physical NIC N, each with its own MAC and IP. Use with `run TAPE &NAME`, or `&+N` for one that lasts as long as the tape."),
    t("tapesync", "Network", "tapesync [HOST] [apply]", "Pull newer tapes from HOST (or every peer) between machines that share a `synckey`; without `apply` only lists."),
    t("gpio", "Network", "gpio @HOST ...", "Drive the GPIO pins of another machine (this one has none)."),
    t("audio", "Machine", "audio | audio device N|auto | audio tone [HZ] [SECONDS] [RATE]", "List audio devices, choose the active one, play a test tone."),
    t("usb", "Machine", "usb [rescan]", "List USB devices; `rescan` finds changes behind hubs and returning disks."),
    t("disks", "Disks", "disks", "List the disks and their ext volumes."),
    t("mount", "Disks", "mount [PATH @LABEL | fstab]", "Show the mount table, add a mount point for a labelled volume, or reload `fstab`."),
    t("umount", "Disks", "umount PATH", "Remove a mount point."),
    t("label", "Disks", "label [diskN|@LABEL [NAME|-]]", "Show or set a volume label (`-` clears it)."),
    t("fsck", "Disks", "fsck [repair] [diskN|@LABEL]", "Check a FOSFS volume; `repair` fixes what it can."),
    t("grow", "Disks", "grow diskN", "Make the disk's FOSFS volume fill the disk; files are kept."),
    t("shrink", "Disks", "shrink diskN [SIZE]  then  shrink diskN confirm", "Shrink the FOSFS partition (SIZE: 512M, 2G; a bare number is MB) to make room for a second partition."),
    t("format", "Disks", "format diskN [NAME] [v3] [force]  |  format diskN exfat [LABEL]  then  format diskN confirm", "Erase a disk and make a FOSFS (v4, or v3) or exFAT volume. Says what it would erase and needs `confirm` as the very next command. Never the boot disk."),
    t("install", "Disks", "install diskN [LABEL] [fosfs=SIZE] [ext4[=LABEL]] [bare] [force]  then  install diskN confirm", "Make a disk bootable with this system. Announces first; needs `confirm` next. Never the boot disk."),
    t("ext4", "Disks", "ext4 diskN [ls|cat|info|recover|trim|mkfs|remkfs|mkdir|rm|mv|write] [PATH ...]", "Work on an ext2/3/4 volume; `ext4 diskN` alone shows it. mkfs and remkfs ask for confirmation."),
    t("sector", "Disks", "sector diskN LBA [COUNT <= 4]", "Dump raw sectors, read-only (decimal or 0x hex)."),
    t("bootcode", "Disks", "bootcode | bootcode update  then  bootcode update confirm", "Show or replace the BIOS boot code / UEFI loader (needs stage1.bin and stage2.bin or loader.efi on the boot volume)."),
    t("kup", "Machine", "kup serial", "Receive a kernel update over the serial line (sender: tools/send_kernel_serial.py)."),
    t("recv", "Machine", "recv NAME", "Receive a file over the serial line (sender: tools/send_file_serial.py PORT FILE NAME)."),
    t("tape", "Tapes and services", "tape [run] NAME [ARGS] [&N]", "Older spelling of `run`."),
    t("run", "Tapes and services", "run NAME [ARGS] [&N|&NAME|&+N]", "Run a tape. In the foreground Escape stops it; with `&N` it is a service owning NIC N (`&NAME` a vNIC, `&+N` a temporary vNIC on N). `NAME help` shows the tape's usage; `NAME @FILE` takes its settings from settings/FILE."),
    t("kill", "Tapes and services", "kill PID", "Stop a process (the number `run` printed, or `ps` shows)."),
    t("service", "Tapes and services", "service | service start|stop|restart NAME|all | service reload", "Named background tapes from `settings/services` (lines `NAME [restart] TAPE ARGS... &NIC`). `restart` ones come back after a crash."),
    t("start", "Tapes and services", "start", "Run the boot script `settings/start` again."),
    t("settings", "Tapes and services", "settings migrate", "Move machine-specific files (*.cfg, smbusers, services) from the root into settings/."),
    t("debug", "Debug", "debug slots|revert|colors|crash now|hang now|deadhang now|deadlock now", "Test hooks: slot report, undo a kernel update, console colours; crash, hang, deadhang and deadlock stop the machine on purpose."),
    t("help", "System", "help [NAME]", "List the commands, or show the usage of a command or tape."),
    t("reboot", "Machine", "reboot", "Restart the machine."),
    t("reset", "Machine", "reset", "Same as `reboot`."),
    t("shutdown", "Machine", "shutdown", "Power off (ACPI)."),
    t("halt", "Machine", "halt", "Same as `shutdown`."),
];

pub fn find(name: &str) -> Option<&'static Topic> {
    TOPICS.iter().find(|topic| topic.name == name)
}

/// `help NAME` for a builtin: its usage and what it does.
pub fn show(topic: &Topic) {
    crate::println!("{}", topic.usage);
    crate::println!("  {}", topic.summary);
}

/// `help` alone: the commands by group, the shared file commands from their own table, and how to get more.
pub fn overview(shared: &[fosfor_shell::Command]) {
    crate::println!("Commands (`help NAME` shows one command or a tape's usage):");
    for group in GROUPS {
        let mut line = alloc::string::String::new();
        let mut label = alloc::format!("{group:<19}");
        for name in shared.iter().map(|command| command.name).filter(|_| *group == "Files").chain(TOPICS.iter().filter(|topic| topic.group == *group).map(|topic| topic.name)) {
            if label.len() + line.len() + name.len() + 1 > 79 {
                crate::println!("{label}{}", line.trim_end());
                label = alloc::string::String::from("                   ");
                line.clear();
            }
            line.push_str(name);
            line.push(' ');
        }
        if !line.is_empty() {
            crate::println!("{label}{}", line.trim_end());
        }
    }
    crate::println!("A tape runs as `NAME [ARGS]` or `run NAME [ARGS]`; `help NAME` shows its usage.");
    crate::println!("`run NAME ARGS &N` runs it as a service on NIC N. Disk commands ask first.");
}
