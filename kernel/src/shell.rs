//! The aarch64 shell: the line editor (`lineedit`, shared with x86) on the serial console, and the commands that need
//! only what the port has so far: files on FOSFS volumes, disks, memory, time. The x86 shell (`term.rs`) is tied to the
//! network terminal and the PC drivers; this one grows towards the same command set (tapes next) as modules are ported.

use fosfor_shell::Command;

use crate::shellhost::KernelHost;
use crate::{lineedit, memory, storage, timer};

/// Prompts, then serves the serial console forever: poll a byte, edit, run a finished line.
static mut START_SCRIPT: [u8; 4096] = [0; 4096];

/// Runs the boot volume's `start` file, one command per line (blank lines and `#` lines skipped), as on x86. A tape in it
/// holds the shell until it ends (Esc), so a long-running one belongs on the last line.
pub(crate) fn start_script(announce_missing: bool) {
    let Some(volume) = storage::boot_volume() else { return };
    let buffer = unsafe { &mut *core::ptr::addr_of_mut!(START_SCRIPT) };
    let length = match volume.read(b"start", buffer) {
        Ok(Some(length)) => length,
        Ok(None) => return if announce_missing { crate::println!("start: no start file on the boot volume.") },
        Err(error) => return crate::println!("start: cannot be read ({error:?})."),
    };
    let script = core::str::from_utf8(&buffer[..length]).unwrap_or("");
    let lines = || script.lines().map(str::trim).filter(|line| !line.is_empty() && !line.starts_with('#') && *line != "start");
    crate::println!("start file: running {} command(s).", lines().count());
    for line in lines() {
        crate::println!("start> {line}");
        execute(line);
    }
}

pub fn run() -> ! {
    lineedit::reset();
    start_script(false);
    crate::print(format_args!("\nType `help` for the commands.\nfos> "));
    loop {
        crate::usb_kbd::poll();
        match crate::arch::aarch64::pl011::read_byte().or_else(crate::usb_kbd::pop) {
            Some(byte) => {
                let mut line = [0u8; lineedit::LINE_MAX];
                if let Some(length) = lineedit::feed(byte, &mut line) {
                    if let Ok(text) = core::str::from_utf8(&line[..length]) {
                        execute(text.trim());
                    }
                    crate::print(format_args!("fos> "));
                }
            }
            // Background tapes (`run NAME &N`) get a pass; the CPU sleeps only when they are all idle.
            // The 100 Hz timer interrupt wakes this at least every 10 ms.
            None => {
                // Frames for the network service (discovery, kernel update) first; more may be waiting.
                if crate::netservice::poll() {
                    continue;
                }
                if !crate::tasks::pass() {
                    crate::cpu::sleep();
                }
            }
        }
    }
}

/// The Pi's own commands; the file commands and `help` come from the shared shell (`crates/shell`).
static COMMANDS: &[Command] = &[
    Command::new("display", "display ...", "the screens.", |_, rest| display(rest)),
    Command::new("acpi", "acpi", "lists the ACPI tables.", |_, _| {
        crate::arch::aarch64::acpi::list(|signature, address, length| {
            crate::println!("{} at {:#x}, {} bytes", core::str::from_utf8(&signature).unwrap_or("????"), address, length)
        })
    }),
    Command::new("recv", "recv ...", "receives a file over the serial port.", |_, rest| recv_serial(rest)),
    Command::new("debug", "debug [status|slots]", "kernel-update setting and the A/B slots (same wording as the x86 kernel).", |_, rest| match rest {
        "" | "status" | "slots" => {
            crate::println!("kernel-update {}", crate::netservice::kernel_update_enabled());
            crate::slots::show();
        }
        _ => crate::println!("debug: status | slots"),
    }),
    Command::new("irqs", "irqs", "interrupt counts.", |_, _| {
        let (uart, dropped) = crate::arch::aarch64::pl011::interrupt_counts();
        crate::println!("timer {}, uart {uart} (bytes dropped {dropped}), sd {}, genet {}", crate::gic::irq_ticks(), crate::sdhci::interrupt_count(), crate::genet::interrupt_count());
        for vector in 0..crate::msi::COUNT as u8 {
            if crate::msi::count(vector) != 0 {
                crate::println!("msi {vector}: {}", crate::msi::count(vector));
            }
        }
    })
    .alias(&["irqstat"]),
    Command::new("peek", "peek ADDR [N]", "reads memory (hex; bring-up).", |_, rest| peek(rest)),
    Command::new("poke", "poke ADDR VALUE", "writes memory (hex; bring-up).", |_, rest| poke(rest)),
];

fn execute(line: &str) {
    if line.is_empty() || fosfor_shell::execute(&mut KernelHost, &[COMMANDS, crate::shellcmds::COMMANDS, fosfor_shell::files::COMMANDS], line) {
        return;
    }
    if !fosfor_shell::external(&mut KernelHost, line) {
        crate::println!("{}: unknown command (try `help`).", line.split_whitespace().next().unwrap_or(""));
    }
}


/// `run NAME [ARGS]`: loads a tape from the boot volume and runs it until it returns or Escape stops it.
pub(crate) fn run_tape(rest: &str) {
    // `run NAME ARGS &N`: a background tape (service) that owns NIC N.
    let (rest, service) = crate::tasks::split_suffix(rest);
    if service == Some(Err(())) {
        return crate::tasks::usage();
    }
    let (name, arguments) = rest.split_once(' ').map_or((rest, ""), |(name, arguments)| (name, arguments.trim()));
    if name.is_empty() {
        crate::println!("run: which tape? (run NAME [ARGS])");
        return;
    }
    // `run NAME help`: a tape with a `.fos_help` section shows it instead of running.
    if matches!(arguments, "help" | "-h" | "--help" | "?") && fosfor_shell::Host::tape_help(&mut KernelHost, name) {
        return;
    }
    if let Some(Ok(nic)) = service {
        return crate::tasks::start(name, arguments, nic);
    }
    if let Some((first, second)) = rest.split_once(" + ") {
        fn split(part: &str) -> (&str, &str) {
            let part = part.trim();
            part.split_once(' ').map_or((part, ""), |(name, arguments)| (name, arguments.trim()))
        }
        let (first, second) = (split(first), split(second));
        let loaded = crate::tape_aarch64::load_slot(first.0, 0).and_then(|one| Ok((one, crate::tape_aarch64::load_slot(second.0, 1)?)));
        // `calc 3 4 + 6 *` has a `+` too: it is a second tape only when the word after it names a tape that loads.
        if let Ok((one, two)) = loaded {
            match crate::tape_aarch64::run_two([(one, first.1), (two, second.1)]) {
                Ok(()) => crate::println!("Both tapes returned; back at the fos shell."),
                Err(reason) => crate::println!("run: {reason}."),
            }
            return;
        }
    }
    match crate::tape_aarch64::load(name).and_then(|entry| crate::tape_aarch64::run(entry, arguments)) {
        Ok(()) => crate::println!("Tape returned; back at the fos shell."),
        Err(reason) => crate::println!("run: {name}: {reason}."),
    }
}

/// Applies the `settings` file's `keymap us|sv` line from the boot volume (the only setting this port reads so far).
/// Reads the `settings` file; returns whether it says `no-msi` (leave PCIe devices on polling).
pub fn apply_settings() -> bool {
    let Some(volume) = storage::boot_volume() else { return false };
    let mut buffer = [0u8; 1024];
    let Ok(Some(length)) = volume.read(b"settings", &mut buffer) else { return false };
    let mut no_msi = false;
    for line in core::str::from_utf8(&buffer[..length]).unwrap_or("").lines() {
        let mut words = line.split_whitespace();
        let first = words.next();
        no_msi |= first == Some("no-msi");
        if first == Some("keymap") {
            match words.next().and_then(crate::usb_kbd::layout_named) {
                Some(layout) => {
                    crate::usb_kbd::set_layout(layout);
                    crate::println!("settings: {}.", line.trim());
                }
                None => crate::println!("settings: `{}` ignored (keymap us or sv).", line.trim()),
            }
        }
    }
    no_msi
}

fn hex(word: Option<&str>) -> Option<u64> {
    u64::from_str_radix(word?.trim_start_matches("0x"), 16).ok()
}

/// `peek ADDR [N]`: N 32-bit words (default 4, at most 64) from a physical address, mapped as device memory on demand.
/// A bring-up tool: an address with nothing behind it faults and the exception report says so.
fn peek(rest: &str) {
    let mut words = rest.split_whitespace();
    let Some(address) = hex(words.next()).map(|address| address & !3) else {
        crate::println!("peek ADDR [N]  (hex)");
        return;
    };
    let count = words.next().and_then(|word| word.parse::<u64>().ok()).unwrap_or(4).clamp(1, 64);
    crate::arch::aarch64::paging::map_device(address, count * 4);
    for index in 0..count {
        if index % 4 == 0 {
            crate::print(format_args!("{}{:#012x}:", if index == 0 { "" } else { "\n" }, address + index * 4));
        }
        let value = unsafe { core::ptr::read_volatile((address + index * 4) as *const u32) };
        crate::print(format_args!(" {value:08x}"));
    }
    crate::println!();
}

/// `poke ADDR VALUE`: one 32-bit device write.
fn poke(rest: &str) {
    let mut words = rest.split_whitespace();
    let (Some(address), Some(value)) = (hex(words.next()).map(|address| address & !3), hex(words.next())) else {
        crate::println!("poke ADDR VALUE  (hex)");
        return;
    };
    crate::arch::aarch64::paging::map_device(address, 4);
    unsafe { core::ptr::write_volatile(address as *mut u32, value as u32) };
}

/// The console UART as a `kup_serial` link.
pub(crate) struct Console;

impl crate::kup_serial::Link for Console {
    fn read_byte(&mut self) -> Option<u8> {
        crate::arch::aarch64::pl011::read_byte()
    }
    fn write_byte(&mut self, byte: u8) {
        crate::arch::aarch64::pl011::write_raw(byte);
    }
    fn micros(&self) -> u64 {
        timer::micros()
    }
}

/// `recv NAME [BAUD]`: takes one file from `tools/send_file_serial.py` over the console UART (same framing and CRC as
/// `kup serial`) and writes it as NAME (or diskN/NAME). Puts a tape on a Pi with no network file service and no SD swap.
fn recv_serial(rest: &str) {
    let mut words = rest.split_whitespace();
    let Some(name) = words.next() else { return crate::println!("Usage: recv NAME [BAUD]   (then run tools/send_file_serial.py PORT FILE NAME)") };
    let baud = words.next().map_or(0, |word| word.parse::<u32>().unwrap_or(1));
    if baud != 0 && !(9_600..=3_000_000).contains(&baud) {
        return crate::println!("recv: BAUD is 9600 to 3000000 (the sender must be told the same).");
    }
    const STAGE_BYTES: usize = 2 * 1024 * 1024;
    let Some(staging) = memory::allocate_contiguous(STAGE_BYTES as u64 / memory::FRAME, None) else {
        return crate::println!("recv: no memory.");
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(staging as *mut u8, STAGE_BYTES) };
    if baud == 0 {
        crate::println!("KUP serial: ready for file {name} (the sender starts within 30 s).");
    } else {
        crate::println!("KUP serial: ready for file {name} at {baud} baud (the sender switches too and starts within 30 s).");
        crate::arch::aarch64::pl011::drain();
        timer::busy_wait_ms(50);
        if !crate::arch::aarch64::pl011::set_baud(baud) {
            memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
            return crate::println!("recv: that speed is not possible with this UART clock.");
        }
    }
    let result = crate::kup_serial::serve_file(&mut Console, buffer, |data| match crate::tape_fs::write(name.as_bytes(), data) {
        0 => Ok(()),
        _ => Err("the file could not be written (bad name, no space, or no such volume)"),
    });
    if baud != 0 {
        crate::arch::aarch64::pl011::drain();
        crate::arch::aarch64::pl011::set_baud(0);
    }
    memory::free(staging, STAGE_BYTES as u64 / memory::FRAME);
    match result {
        Ok(length) => crate::println!("recv: {length} bytes written to {name}."),
        Err(message) => crate::println!("recv: {message}; nothing was written."),
    }
}

/// `display` asks the Pi's VideoCore firmware how many HDMI outputs it drives and what each is set to (read-only: it selects
/// a display for the questions, then selects display 0 again). Meaningful on a Pi only; elsewhere the mailbox does not answer.
fn display(rest: &str) {
    use crate::mailbox::*;
    if crate::arch::aarch64::gic::bases().0 != 0xff84_1000 {
        println!("display: the VideoCore mailbox exists on a Raspberry Pi 4 only.");
        return;
    }
    if rest.starts_with("test") {
        let display: u32 = rest[4..].trim().parse().unwrap_or(1);
        match framebuffer(display, 1920, 1080) {
            Some((base, size, pitch, _, _)) => {
                println!("display {display}: framebuffer {base:#x}, {size} bytes, pitch {pitch}; drawing colour bars.");
                crate::paging::map_framebuffer(base, size);
                let (width, rows) = (pitch as u64 / 4, size / u64::from(pitch));
                let colours = [0xffffff, 0xffff00, 0x00ffff, 0x00ff00, 0xff00ff, 0xff0000, 0x0000ff, 0x808080];
                for y in 0..rows {
                    for x in 0..width {
                        let colour = colours[(x * 8 / width) as usize];
                        unsafe { ((base + (y * width + x) * 4) as *mut u32).write_volatile(colour) };
                    }
                }
            }
            None => println!("display {display}: the firmware gave no framebuffer."),
        }
        return;
    }
    let mut count = [0u32];
    if tag(GET_NUM_DISPLAYS, &[], &mut count).is_none() {
        println!("display: no VideoCore mailbox answer (not a Pi, or the tag is unknown).");
        return;
    }
    println!("display: firmware reports {} display(s).", count[0]);
    for index in 0..count[0].clamp(2, 4) {
        let mut selected = [0u32];
        if tag(SET_DISPLAY_NUM, &[index], &mut selected).is_none() {
            println!("  {index}: select refused");
            continue;
        }
        let mut id = [0u32];
        let id_known = tag(GET_DISPLAY_ID, &[index], &mut id).is_some();
        println!("  {index}: select answered {}, display id {}", selected[0], if id_known { id[0] as i64 } else { -1 });
        let (mut physical, mut virtual_size, mut depth, mut pitch) = ([0u32; 2], [0u32; 2], [0u32], [0u32]);
        let ok = tag(GET_PHYSICAL_SIZE, &[], &mut physical).is_some()
            && tag(GET_VIRTUAL_SIZE, &[], &mut virtual_size).is_some()
            && tag(GET_DEPTH, &[], &mut depth).is_some()
            && tag(GET_PITCH, &[], &mut pitch).is_some();
        if ok {
            println!("  {index}: {}x{} (buffer {}x{}), {} bpp, pitch {}", physical[0], physical[1], virtual_size[0], virtual_size[1], depth[0], pitch[0]);
        } else {
            println!("  {index}: selected, but the size tags got no answer");
        }
    }
    let _ = tag(SET_DISPLAY_NUM, &[0], &mut [0u32]);
}

