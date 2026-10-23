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
        crate::crash::pet();
        crate::crash::housekeeping();
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
    Command::new("cpufreq", "cpufreq [MHZ|min|max]", "the ARM clock, SoC temperature and throttle flags; sets the clock.", |_, rest| crate::pipower::command(rest)).alias(&["temp"]),
    Command::new("i2s", "i2s probe|capture [SECONDS]|audio on|off", "the Maltex I2S audio HAT: clock edge counts, a level meter, or offer it as an audio device.", |_, rest| crate::i2s_pi::command(rest)),
    Command::new("bootcode", "bootcode [update] [diskN] [confirm]", "shows or replaces the UEFI loader (loader.efi on disk1).", |_, rest| crate::bootcode::command(rest.split_whitespace())),
    Command::new("i2c", "i2c [status|speed KHZ|scan|read ADDR [N]|write ADDR BYTE...|get ADDR REG [N]]", "the header's I2C bus (GPIO2/3): scan, read and write devices.", |_, rest| crate::i2c::command(rest)),
    Command::new("recv", "recv ...", "receives a file over the serial port.", |_, rest| recv_serial(rest)),
    Command::new("peek", "peek ADDR [N]", "reads memory (hex; bring-up).", |_, rest| peek(rest)),
    Command::new("poke", "poke ADDR VALUE", "writes memory (hex; bring-up).", |_, rest| poke(rest)),
];

fn execute(line: &str) {
    if line.split_whitespace().next() != Some("format") {
        crate::formatshell::cancel();
    }
    if line.split_whitespace().next() != Some("bootcode") {
        crate::bootcode::cancel();
    }
    if line.is_empty() || fosfor_shell::execute(&mut KernelHost, &[COMMANDS, crate::shellcmds::COMMANDS, fosfor_shell::files::COMMANDS], line) {
        return;
    }
    if !fosfor_shell::external(&mut KernelHost, line) {
        crate::println!("{}: unknown command (try `help`).", line.split_whitespace().next().unwrap_or(""));
    }
}


/// A tape name and its arguments from one part of a `run` line.
fn tape_part(part: &str) -> (&str, &str) {
    let part = part.trim();
    part.split_once(' ').map_or((part, ""), |(name, arguments)| (name, arguments.trim()))
}

/// `run A | B screen=P &N`: two background tapes share physical screen P (default 0); A owns NIC N, B none.
fn start_pair(rest: &str, nic: Option<usize>) -> bool {
    let Some((first, second)) = rest.split_once(" | ") else { return false };
    let ((one, one_args), (two, two_args)) = (tape_part(first), tape_part(second));
    let phys = crate::tasks::group_screen(one_args, two_args).unwrap_or(0);
    if phys > 1 {
        crate::println!("run: there is no screen {phys}.");
        return true;
    }
    let mode = crate::tasks::split_mode(one_args, two_args);
    let (mut one_text, mut two_text) = ([0u8; 256], [0u8; 256]);
    let (Some(one_args), Some(two_args)) = (
        crate::tasks::with_screen(&mut one_text, one_args, 2 + 2 * phys, true),
        crate::tasks::with_screen(&mut two_text, two_args, 3 + 2 * phys, true),
    ) else {
        crate::println!("run: the arguments of a tape take at most 240 bytes.");
        return true;
    };
    crate::framebuffer::split_set_service(phys, mode);
    crate::tasks::start(one, one_args, nic);
    crate::tasks::start(two, two_args, None);
    true
}

/// `run A + B` (two screens), `run A | B` (one screen shared) and up to four tapes as `run A | B + C | D`. Every tape is
/// loaded first: `calc 3 4 + 6 *` has a `+` too, and is no such line unless each word after a separator names a tape. A
/// pair shares the screen named by a `screen=P` among its arguments (default: the pair's place in the line). False: this
/// is not such a line.
fn run_groups(rest: &str) -> bool {
    let mut tapes = [("", ""); 4];
    let mut sizes = [0usize; 2];
    let (mut count, mut groups) = (0, 0);
    for (group, text) in rest.split(" + ").enumerate() {
        if group > 1 {
            return false;
        }
        groups += 1;
        for (position, part) in text.split(" | ").enumerate() {
            if position > 1 {
                return false;
            }
            tapes[count] = tape_part(part);
            sizes[group] += 1;
            count += 1;
        }
    }
    if count < 2 {
        return false;
    }
    let mut entries = [0u64; 4];
    for index in 0..count {
        match crate::tape_aarch64::load_slot(tapes[index].0, index) {
            Ok(entry) => entries[index] = entry,
            Err(_) => return false,
        }
    }
    // A tape alone has its group's physical screen; the halves of a pair are numbered by the kernel.
    let mut texts = [[0u8; 256]; 4];
    let mut args = [""; 4];
    let mut splits = [None; 2];
    let mut turns = [(0usize, None); 2];
    let mut used = [false; 2];
    let mut index = 0;
    let mut slots = texts.iter_mut();
    for group in 0..groups {
        let size = sizes[group];
        let own = tapes[index].1;
        let other = if size == 2 { tapes[index + 1].1 } else { "" };
        turns[group] = (crate::tasks::group_screen(own, other).unwrap_or(group), crate::tasks::rotate_option(own, other));
        if size == 2 {
            let phys = crate::tasks::group_screen(tapes[index].1, tapes[index + 1].1).unwrap_or(group);
            if phys > 1 || used[phys] {
                crate::println!("run: screen {phys} is not available for this pair.");
                return true;
            }
            used[phys] = true;
            splits[group] = Some((phys, crate::tasks::split_mode(tapes[index].1, tapes[index + 1].1)));
        }
        for part in 0..size {
            let screen = if size == 2 { 2 + 2 * splits[group].map_or(0, |(phys, _)| phys) + part } else { group };
            let Some(text) = crate::tasks::with_screen(slots.next().unwrap(), tapes[index + part].1, screen, size == 2) else {
                crate::println!("run: the arguments of a tape take at most 240 bytes.");
                return true;
            };
            args[index + part] = text;
        }
        index += size;
    }
    let mut jobs = [(0u64, ""); 4];
    for index in 0..count {
        jobs[index] = (entries[index], args[index]);
    }
    for (phys, mode) in splits.into_iter().flatten() {
        crate::framebuffer::split_set(phys, Some(mode));
    }
    for (phys, mode) in turns.into_iter().take(groups) {
        crate::framebuffer::rotate_run(phys, mode);
    }
    let outcome = crate::tape_aarch64::run_many(&jobs[..count]);
    crate::framebuffer::split_clear_all();
    crate::framebuffer::rotate_reset();
    match outcome {
        Ok(()) => crate::println!("All {count} tapes returned; back at the fos shell."),
        Err(reason) => crate::println!("run: {reason}."),
    }
    true
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
        if rest.contains(" | ") && start_pair(rest, nic) {
            return;
        }
        return crate::tasks::start(name, arguments, nic);
    }
    if run_groups(rest) {
        return;
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
        if first == Some("i2s-audio") {
            crate::i2s_pi::apply_setting(line);
        }
        if first == Some("pi-clock") {
            crate::pipower::apply_setting(line);
        }
        if matches!(first, Some("gpio-remote" | "gpio-sim" | "gpio-deny")) {
            crate::gpio::apply_setting(line);
        }
        if let Some(key @ ("rotate" | "rotate1")) = first {
            let word = words.next().unwrap_or("");
            let mode = crate::framebuffer::Rotate::parse(word);
            if mode.is_some() || matches!(word, "off" | "none") {
                crate::framebuffer::rotate_default(usize::from(key == "rotate1"), mode);
                crate::println!("settings: {key} {word} (tapes' pixel screens; the text console stays upright).");
            } else {
                crate::println!("settings: `{}` ignored ({key} r|l|off).", line.trim());
            }
        }
        if let Some(key @ ("foreground" | "background")) = first {
            let name = words.next().unwrap_or("");
            let (fg, bg) = if key == "foreground" { (Some(name), None) } else { (None, Some(name)) };
            if crate::framebuffer::set_colors(fg, bg) {
                crate::println!("settings: {key} {name}.");
            } else {
                crate::println!("settings: `{}` ignored (unknown colour, or no pixel console).", line.trim());
            }
        }
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

