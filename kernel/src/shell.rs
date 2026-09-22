use crate::arch::x86::{halt_forever, reset};
use crate::boot_info::{BootInfo, MemoryRegion};
use crate::console;
use crate::cpu;
use crate::disk;
use crate::fosfs;
use crate::interrupts;
use crate::keyboard;
use crate::keyboard::Keyboard;
use crate::nic;
use crate::parallel;
use crate::pmem;
use crate::{print, println};

const LINE_CAPACITY: usize = 256;
const TAPE_ARGUMENT_FILE_CAPACITY: usize = 4096;
const START_FILE_NAME: &[u8] = b"start";
const START_FILE_CAPACITY: usize = 4096;
const TERM_NET_IDLE_TICKS: u64 = 60 * 100;

pub fn run(info: &BootInfo) -> ! {
    let keyboard_available = Keyboard::initialize().is_some();
    interrupts::initialize();
    disk::initialize(info);
    disk::discover_secondary_disks(info);
    if keyboard_available {
        println!("PS/2 keyboard initialized. Type 'help' for commands.");
    } else {
        println!("PS/2 keyboard unavailable; serial input remains available.");
        println!("Type 'help' for commands.");
    }
    if nic::count() > 0 {
        println!("Detected Ethernet interfaces:");
        nic_list();
    }
    run_start_file(info);
    // Automatically wait for a remote `term net` client at every boot,
    // exactly as if a local operator had just typed `term net`: this
    // makes every fos machine remotely reachable by default with no
    // manual step, which matters most for a machine with no attached
    // screen/keyboard at all. A local operator who wants the ordinary
    // interactive shell right away, without waiting, can press F12 (or
    // F4/F6/F7/F8/F10) immediately to give up waiting, exactly as they
    // would to leave a manually started `term net` session; if a remote
    // client does connect first, a remote disconnect returns to waiting and
    // a local escape returns to the local shell below.
    term_net(info);
    let mut editor = LineEditor::new();
    prompt();
    let mut last_activity = interrupts::ticks();

    loop {
        let byte = console::read_serial_byte().or_else(keyboard::pop_char);
        let Some(byte) = byte else {
            if interrupts::ticks().wrapping_sub(last_activity) >= TERM_NET_IDLE_TICKS {
                println!(
                    "\nNo local shell activity for 60 seconds; waiting for a remote terminal client again."
                );
                term_net(info);
                last_activity = interrupts::ticks();
                prompt();
            }
            continue;
        };
        editor.feed(byte, info);
        last_activity = interrupts::ticks();
    }
}

/// Where a `start` script was found (or why it wasn't run), returned by
/// [`locate_start_file`] so both the silent boot-time call and the
/// interactive `start` command can share one lookup while still reporting
/// their own outcomes appropriately.
enum StartFileLookup {
    Found(disk::Device, u64),
    Missing,
    Error(disk::Error),
}

/// Looks for the optional `start` script on the boot disk first, falling
/// back to the first secondary disk (`disk2`, the same disk the "second
/// (non-boot) disk" config/log convention already targets) only if the
/// boot disk has none at all -- letting a deployment keep its boot script
/// alongside other disk2-resident config/log files instead of duplicating
/// it onto disk1 too. Only the boot disk's own result can produce
/// `StartFileLookup::Error`: it always carries a valid FOSFS volume, so a
/// read error there is a real problem worth reporting. disk2, in
/// contrast, is very often attached blank and unformatted (the normal
/// state before a deployment's first `format disk2`) -- `fosfs::validate`
/// fails on any such disk, and treating that the same as a genuine error
/// would print a scary, misleading message on every single boot of a
/// perfectly ordinary machine with an unused secondary disk. So *any*
/// outcome on disk2 other than actually finding the file (including every
/// error) is simply treated as "no start script there", exactly like
/// disk1's own `Ok(None)`.
fn locate_start_file(info: &BootInfo) -> StartFileLookup {
    match fosfs::file_length(info, disk::Device::Boot, START_FILE_NAME) {
        Ok(Some(length)) => return StartFileLookup::Found(disk::Device::Boot, length),
        Ok(None) => {}
        Err(error) => return StartFileLookup::Error(error),
    }
    if disk::secondary_count() == 0 {
        return StartFileLookup::Missing;
    }
    let device = disk::Device::Secondary(0);
    match fosfs::file_length(info, device, START_FILE_NAME) {
        Ok(Some(length)) => StartFileLookup::Found(device, length),
        _ => StartFileLookup::Missing,
    }
}

/// Runs the optional FOSFS `start` script before the automatic remote-terminal
/// wait. Lines use the ordinary shell dispatcher, so a deployment script has
/// exactly the same command syntax and error reporting as an interactive one.
/// Silent when no start file exists on either disk -- a machine with no
/// start script should boot normally with no scary message.
fn run_start_file(info: &BootInfo) {
    match locate_start_file(info) {
        StartFileLookup::Found(device, length) => execute_start_file(info, device, length),
        StartFileLookup::Missing => {}
        StartFileLookup::Error(error) => {
            println!("start: could not inspect the file: {error:?}");
        }
    }
}

/// Interactive `start` command: reruns the exact same optional boot script
/// `run_start_file` runs automatically at boot, letting an operator restart
/// a `tape run`-driven script from the top -- e.g. after pressing F12 to
/// leave a long-running tape it started -- without a full machine reset.
/// Unlike the silent boot-time call, it reports plainly when no start file
/// exists at all, since an operator explicitly asking for one deserves to
/// know why nothing happened.
fn start_command(info: &BootInfo) {
    match locate_start_file(info) {
        StartFileLookup::Found(device, length) => execute_start_file(info, device, length),
        StartFileLookup::Missing => {
            println!("start: no start file found on disk1 or disk2.");
        }
        StartFileLookup::Error(error) => {
            println!("start: could not inspect the file: {error:?}");
        }
    }
}

/// Reads and runs one already-located `start` script from `device`.
fn execute_start_file(info: &BootInfo, device: disk::Device, length: u64) {
    if length > START_FILE_CAPACITY as u64 {
        println!(
            "start: {length}-byte file is too large; the boot script limit is {START_FILE_CAPACITY} bytes."
        );
        return;
    }

    let mut contents = [0u8; START_FILE_CAPACITY];
    let length = match fosfs::read(info, device, START_FILE_NAME, &mut contents[..length as usize]) {
        Ok(Some(length)) => length as usize,
        Ok(None) => return,
        Err(error) => {
            println!("start: could not read the file: {error:?}");
            return;
        }
    };
    println!("start: running {length}-byte boot script.");

    let mut line_start = 0;
    let mut line_number = 1;
    for index in 0..=length {
        if index != length && contents[index] != b'\n' {
            continue;
        }
        let mut line = &contents[line_start..index];
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = trim_start(line);
        if !line.is_empty() && line[0] != b'#' {
            print!("start line {line_number}: ");
            console::write_bytes(line);
            println!();
            execute(line, info);
        }
        line_start = index + 1;
        line_number += 1;
    }
}

/// One command line's editing state: the buffer of characters typed so
/// far, how many of them are in use, and whether the previous byte was a
/// bare `\r` (so a `\r\n` pair from a terminal that sends both is treated
/// as a single Enter, not two). Factored out of `run`'s loop so
/// `term_net` can feed it bytes arriving from a remote network peer
/// through the exact same line-editing and command-dispatch logic the
/// interactive local prompt uses, instead of reimplementing it.
struct LineEditor {
    buffer: [u8; LINE_CAPACITY],
    length: usize,
    saw_carriage_return: bool,
}

impl LineEditor {
    fn new() -> Self {
        LineEditor {
            buffer: [0u8; LINE_CAPACITY],
            length: 0,
            saw_carriage_return: false,
        }
    }

    /// Feeds one input byte through the same line-editing rules the
    /// interactive shell prompt uses -- echoing printable characters,
    /// handling backspace/delete, and running `execute` once Enter is
    /// seen -- regardless of whether `byte` came from the local keyboard,
    /// COM1, or a remote network peer.
    fn feed(&mut self, byte: u8, info: &BootInfo) {
        match byte {
            b'\n' if self.saw_carriage_return => self.saw_carriage_return = false,
            b'\r' | b'\n' => {
                println!();
                execute(&self.buffer[..self.length], info);
                self.length = 0;
                self.saw_carriage_return = byte == b'\r';
                prompt();
            }
            8 | 127 if self.length > 0 => {
                self.saw_carriage_return = false;
                self.length -= 1;
                console::erase_last_character();
            }
            0x20..=0x7e if self.length < LINE_CAPACITY => {
                self.saw_carriage_return = false;
                self.buffer[self.length] = byte;
                self.length += 1;
                print!("{}", byte as char);
            }
            0x20..=0x7e => {
                self.saw_carriage_return = false;
                println!("\nInput limit is {LINE_CAPACITY} characters.")
            }
            _ => {}
        }
    }
}

fn prompt() {
    print!("fos> ");
}

#[inline(never)]
fn vga_command(arguments: &[u8]) {
    let (mode_word, remainder) = take_word(arguments);
    if mode_word.is_empty() {
        let mode = console::current_vga_mode();
        println!(
            "VGA mode: {} ({} columns x {} rows).",
            mode.name(),
            mode.width(),
            mode.height()
        );
        println!("Available modes: 80x25 (default), 80x50, 132x25 (experimental).");
        println!("Use 'vga d' to return to the default 80x25 mode.");
        return;
    }
    if !remainder.is_empty() {
        println!("Usage: vga [d|80x25|80x50|132x25]");
        return;
    }

    let mode = match mode_word {
        b"d" | b"80x25" => console::VgaMode::Text80x25,
        b"80x50" => console::VgaMode::Text80x50,
        b"132x25" => console::VgaMode::Experimental132x25,
        _ => {
            println!("Usage: vga [d|80x25|80x50|132x25]");
            return;
        }
    };
    console::set_vga_mode(mode);
    println!(
        "VGA mode changed to {} ({} columns x {} rows).",
        mode.name(),
        mode.width(),
        mode.height()
    );
    if mode == console::VgaMode::Experimental132x25 {
        println!("Warning: 132-column timing is experimental and may not work");
        println!("on every VGA adapter or monitor.");
    }
}

/// `execute` recurses (indirectly, via the `start`/boot-script path below)
/// when a script line itself dispatches another command: `start_command` ->
/// `execute_start_file` -> `execute` again for each line. Every command
/// handler this function's match arms call is therefore marked
/// `#[inline(never)]`. Without that, this release profile's aggressive
/// whole-program size optimization (`opt-level = "z"`, `lto = true`,
/// `codegen-units = 1`) happily inlines every handler -- `tape_run`'s own
/// frame, `nic_command`'s frame, `term_command`'s frame, and so on --
/// directly into `execute`'s own stack frame, even though only one match
/// arm ever runs per call. That makes one call to `execute` cost the *sum*
/// of every possible command's stack usage, not just the one actually
/// invoked. A single top-level call absorbs that fine, but two of them
/// stacked (typing `start` interactively, which is itself dispatched
/// through `execute`, and which then calls `execute` again for each script
/// line) doubles that inflated frame on the kernel's fixed 32 KiB stack --
/// confirmed, before this fix, to double-fault running a script that
/// merely `tape run`s a large tape (e.g. `router`) via the interactive
/// `start` command specifically, while the exact same commands typed
/// manually, or run automatically at boot (where `execute` is only ever
/// one level deep), worked every time. `#[inline(never)]` keeps `execute`
/// itself a small dispatcher and lets each handler's own frame exist only
/// when that handler is actually on the call stack, exactly as a plain
/// `match` over function calls should behave.
///
/// This alone was not sufficient, though: even with `execute` shrunk to a
/// thin dispatcher, the second, nested `execute` -> `tape_command` ->
/// `tape_run` chain the interactive `start` path adds is a genuine,
/// unavoidable extra layer of stack depth on top of whatever the running
/// tape itself uses -- and a heavy tape like `router` (multiple NICs,
/// DHCP, health checks, NAT) still exhausted the kernel's original 32 KiB
/// stack at that combined depth. `kernel/src/entry.S`'s `kernel_stack` was
/// doubled to 64 KiB to give this path real headroom; see its comment for
/// why that budget is safe against `linker.ld`'s low-memory-region limit.
fn execute(command: &[u8], info: &BootInfo) {
    let (name, arguments) = take_word(command);
    match name {
        b"" => {}
        b"help" => {
            // Every line here is kept to at most 79 columns (VGA text mode
            // wraps a line at exactly 80, and a println! whose text lands
            // precisely on column 80 wraps *and* leaves a spurious blank
            // row for its own trailing '\n' -- so 79 is the true safe
            // maximum, not 80) and the whole listing to 13 lines, so it
            // always fits a standard 80x25 screen without wrapping or
            // scrolling any of itself out of view. Prefer trimming detail
            // (full syntax remains in README.md/AGENTS.md) over adding a
            // line or letting one run long if this command ever grows.
            //
            // Disabled while connected via `term_printer`: its own output
            // would otherwise be captured and printed like any other
            // command's, and this listing is long enough that running it
            // by habit would waste real paper every time. `print run
            // help` is a deliberate, always-available exception (it
            // temporarily clears this flag around its own nested
            // `execute` call -- see `print_run`) for getting exactly one
            // paper copy, including from inside an active session.
            if unsafe { PRINTER_SESSION_ACTIVE } {
                println!("help is disabled while connected to a printer terminal (it would");
                println!("print this whole listing). Run 'print run help' once instead, or");
                println!("check the manual.");
                return;
            }
            if no_arguments(arguments) {
                println!(
                    "System: help clear vga about bootinfo memmap memory cpuinfo hexdump halt reset"
                );
                println!("Interrupts: ticks uptime irqstat pmem palloc pfree");
                println!(
                    "Disk: diskinfo/readsector/format/ls/cat/write/rm [diskN], copy SRC NAME DST"
                );
                println!(
                    "      (disk1 is the boot disk, N may be omitted; disk2, disk3... are extras)"
                );
                println!(
                    "      start (rerun the boot 'start' script, e.g. after F12 from a tape)"
                );
                println!(
                    "Serial: serial (list ports), term [PORT [BAUD]] (raw terminal, F12 to exit)"
                );
                println!(
                    "Ethernet: nic [list|use N|send|sniff|discover|peers], chat [PEER_MAC|N]"
                );
                println!("Tape: tape status | tape run [ARGS...|@FILE|@diskN/FILE]");
                println!(
                    "      tape load disk[N] NAME | tape load net | tape save [diskN] NAME"
                );
                println!("      tape send NAME PEER");
                println!(
                    "Remote: term net (wait for a remote shell over Ethernet; same as at boot)"
                );
                println!(
                    "Printer: printer (list LPTs), term printer|LPTn (raw, F12 exits, F2 kills"
                );
                println!(
                    "      the current line), print [diskN] NAME | print run CMD..."
                );
                println!(
                    "Note: F12 (or F4/F6/F7/F8/F10) gives up on any blocking wait (sniff/term/chat)."
                );
            }
        }
        b"clear" if no_arguments(arguments) => console::clear(),
        b"vga" => vga_command(arguments),
        b"about" if no_arguments(arguments) => {
            println!("fos is a small educational 32-bit x86 operating system.")
        }
        b"bootinfo" if no_arguments(arguments) => print_boot_info(info),
        b"memmap" if no_arguments(arguments) => print_memory_map(info),
        b"memory" if no_arguments(arguments) => print_memory_summary(info),
        b"cpuinfo" if no_arguments(arguments) => print_cpu_info(),
        b"diskinfo" => {
            let (device, remainder) = parse_disk_only(arguments);
            if no_arguments(remainder) {
                print_disk_info(info, device);
            }
        }
        b"ticks" if no_arguments(arguments) => print_ticks(),
        b"uptime" if no_arguments(arguments) => print_uptime(),
        b"irqstat" if no_arguments(arguments) => print_irq_stats(),
        b"pmem" if no_arguments(arguments) => print_pmem_stats(),
        b"palloc" if no_arguments(arguments) => palloc_command(),
        b"pfree" => pfree_command(arguments),
        b"readsector" => read_sector_command(info, arguments),
        b"hexdump" => hexdump_command(info, arguments),
        b"format" => {
            let (device, remainder) = parse_disk_only(arguments);
            if no_arguments(remainder) {
                filesystem_result(fosfs::format(info, device), "FOSFS formatted.")
            }
        }
        b"ls" => {
            let (device, remainder) = parse_disk_only(arguments);
            if no_arguments(remainder) {
                filesystem_result(fosfs::list(info, device), "")
            }
        }
        b"cat" => cat_command(info, arguments),
        b"write" => write_command(info, arguments),
        b"rm" => remove_command(info, arguments),
        b"copy" => copy_command(info, arguments),
        b"start" if no_arguments(arguments) => start_command(info),
        b"term" => term_command(info, arguments),
        b"serial" if no_arguments(arguments) => print_serial_ports(),
        b"printer" if no_arguments(arguments) => print_printer_ports(),
        b"print" => print_command(info, arguments),
        b"nic" => nic_command(arguments),
        b"chat" => chat_command(arguments),
        b"tape" => tape_command(info, arguments),
        b"halt" => {
            if !no_arguments(arguments) {
                return;
            }
            println!("Halting CPU. Reset or power-cycle to restart.");
            halt_forever();
        }
        b"reset" => {
            if !no_arguments(arguments) {
                return;
            }
            println!("Resetting the machine now (8042 keyboard-controller pulse reset)...");
            reset();
        }
        b"clear" | b"about" | b"bootinfo" | b"memmap" | b"memory" | b"cpuinfo" | b"ticks"
        | b"uptime" | b"irqstat" | b"pmem" | b"palloc" | b"serial" | b"printer" | b"start" => {
            println!("This command does not accept arguments.");
        }

        _ => println!("Unknown command. Type 'help' for commands."),
    }
}

fn print_ticks() {
    println!(
        "Timer ticks: {} (at {} Hz)",
        interrupts::ticks(),
        interrupts::tick_frequency_hz()
    );
}

fn print_uptime() {
    let ticks = interrupts::ticks();
    let frequency = u64::from(interrupts::tick_frequency_hz());
    println!(
        "Uptime: {}.{:02} seconds ({} ticks)",
        ticks / frequency,
        (ticks % frequency) * 100 / frequency,
        ticks
    );
}

fn print_irq_stats() {
    let (queued_scancodes, dropped_scancodes) = interrupts::scancode_queue_stats();
    println!(
        "IRQ0 (timer): {} fired; IRQ1 (keyboard): {} fired ({} queued,",
        interrupts::timer_irq_count(),
        interrupts::keyboard_irq_count(),
        queued_scancodes,
    );
    println!(
        "{} dropped); spurious IRQ7/IRQ15: {} fired",
        dropped_scancodes,
        interrupts::spurious_irq_count()
    );
    let keyboard_restore_failures = keyboard::bios_restore_failures();
    if keyboard_restore_failures != 0 {
        println!("PS/2 Scan Code Set 1 restoration after BIOS disk calls failed");
        println!("{keyboard_restore_failures} time(s).");
    }
    for irq in 3..16 {
        if interrupts::pci_irq_registered(irq) {
            println!(
                "PCI IRQ{irq}: {} dispatch(es) (device handlers only acknowledge",
                interrupts::pci_irq_count(irq)
            );
            println!("and defer receive work).");
        }
    }
}

fn print_pmem_stats() {
    let (total, free) = pmem::stats();
    let used = total - free;
    println!(
        "Physical frames: {total} total, {free} free, {used} used ({} KiB/frame)",
        pmem::FRAME_SIZE / 1024
    );
}

fn palloc_command() {
    match pmem::allocate() {
        Some(address) => println!("Allocated frame at 0x{address:08x}."),
        None => println!("No free physical frames remain."),
    }
}

fn pfree_command(arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(address) = parse_number(value) else {
        println!("Usage: pfree ADDRESS (as returned by palloc)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: pfree ADDRESS");
        return;
    }
    pmem::free(address);
    println!("Frame at 0x{address:08x} released (if it was allocated and in range).");
}

fn print_cpu_info() {
    let vendor = cpu::vendor();
    print!("CPUID vendor: ");
    console::write_bytes(&vendor);
    let (edx, ecx) = cpu::features();
    println!("\nCPUID feature bits: EDX=0x{edx:08x} ECX=0x{ecx:08x}");
}

/// Display names for the four legacy COM ports, in BIOS Data Area index
/// order; shared by `serial` (listing) and `term` (port selection).
const SERIAL_PORT_NAMES: [&[u8]; 4] = [b"COM1", b"COM2", b"COM3", b"COM4"];

/// Display names for PCI-discovered serial ports (see `console::detect_pci_ports`
/// / `pci::find_serial_ports`), in discovery order.
const PCI_PORT_NAMES: [&[u8]; console::MAX_PCI_SERIAL_PORTS] = [b"PCI0", b"PCI1", b"PCI2", b"PCI3"];

/// Display names for the four legacy LPT ports, in BIOS Data Area index
/// order; shared by `printer` (listing) and `term` (port selection). ISA
/// only -- see `parallel`'s module doc comment for why there is no PCI
/// parallel-card discovery to mirror `PCI_PORT_NAMES` above.
const PRINTER_PORT_NAMES: [&[u8]; 4] = [b"LPT1", b"LPT2", b"LPT3", b"LPT4"];

fn print_serial_ports() {
    let ports = console::detect_ports();
    for (index, name) in SERIAL_PORT_NAMES.iter().enumerate() {
        let name = core::str::from_utf8(name).unwrap_or("?");
        match ports[index] {
            Some(base) if index == 0 => {
                println!("{name}: 0x{base:04x} (local shell console, fixed at 115200 8N1)");
            }
            Some(base) => {
                println!("{name}: 0x{base:04x} (free for 'term'; baud chosen when entering it)")
            }
            None => println!("{name}: not detected"),
        }
    }
    let (pci_ports, pci_count) = console::detect_pci_ports();
    if pci_count == 0 {
        println!("PCI: no additional serial controllers found on the PCI bus.");
    } else {
        for index in 0..pci_count {
            let name = core::str::from_utf8(PCI_PORT_NAMES[index]).unwrap_or("?");
            let base = pci_ports[index];
            println!(
                "{name}: 0x{base:04x} (found via PCI enumeration; free for 'term';"
            );
            println!("baud chosen when entering it)");
        }
    }
}

fn print_printer_ports() {
    let ports = parallel::detect_ports();
    for (index, name) in PRINTER_PORT_NAMES.iter().enumerate() {
        let name = core::str::from_utf8(name).unwrap_or("?");
        match ports[index] {
            Some(base) => println!("{name}: 0x{base:04x} (free for 'term printer'/'term {name}')"),
            None => println!("{name}: not detected"),
        }
    }
}

/// The first detected LPT port, for `term printer`/`print`'s "no port
/// named explicitly" default -- mirrors how bare `term` defaults to COM1.
fn default_printer_port() -> Option<(&'static [u8], u16)> {
    let ports = parallel::detect_ports();
    let index = ports.iter().position(Option::is_some)?;
    Some((PRINTER_PORT_NAMES[index], ports[index].unwrap()))
}

/// One human-readable line per [`parallel::PrinterFault`], shown once when
/// a fault first appears (not on every retry -- see `drain_print_relay`)
/// and reused by both `term_printer` and `print_command`.
fn printer_fault_message(fault: parallel::PrinterFault) -> &'static str {
    match fault {
        parallel::PrinterFault::NotSelected => {
            "Printer offline (not selected); waiting... (F12 to give up)"
        }
        parallel::PrinterFault::PaperOut => "Printer out of paper; waiting... (F12 to give up)",
        parallel::PrinterFault::ErrorLine => {
            "Printer reports an error condition; waiting... (F12 to give up)"
        }
        parallel::PrinterFault::Busy => {
            "Printer busy longer than expected; still waiting... (F12 to give up)"
        }
    }
}

/// Drains the entire print-relay queue to `printer`, one byte at a time,
/// via [`parallel::Printer::write_byte`]. A fault (paper out, offline, a
/// latched error, or an unexpectedly long busy spell) is reported once and
/// then retried with the *same* byte -- never skipped or dropped -- until
/// it clears or the local operator presses F12 to give up, satisfying
/// "pause, don't abort" (see `term_printer`'s doc comment) without any
/// single call blocking forever: each retry is its own bounded
/// `write_byte` attempt, and F12 is rechecked between every one.
///
/// Returns `false` if the operator gave up mid-fault, leaving whatever was
/// still queued undelivered (silently dropped, exactly like every other
/// best-effort output queue in this kernel -- see `console::relay_push`'s
/// own doc comment).
fn drain_print_relay(printer: &parallel::Printer) -> bool {
    let mut chunk = [0u8; 256];
    let mut last_fault: Option<parallel::PrinterFault> = None;
    loop {
        let queued = console::peek_print_relay(&mut chunk);
        if queued == 0 {
            return true;
        }
        let mut written = 0;
        while written < queued {
            match printer.write_byte(chunk[written]) {
                Ok(()) => {
                    if last_fault.take().is_some() {
                        println!("Printer recovered; resuming.");
                    }
                    written += 1;
                }
                Err(fault) => {
                    if last_fault != Some(fault) {
                        println!("{}", printer_fault_message(fault));
                        last_fault = Some(fault);
                    }
                    if matches!(keyboard::poll_term_event(), Some(keyboard::KeyEvent::Escape)) {
                        console::discard_print_relay(written);
                        return false;
                    }
                }
            }
        }
        console::discard_print_relay(written);
    }
}

/// `print [diskN] NAME` prints one FOSFS file's raw contents; `print run
/// CMD...` runs an ordinary command and prints only its output (never the
/// echoed command line itself, unlike `term_printer`'s own transcript) --
/// the intended way to get a one-off paper copy of something like `help`
/// without leaving it permanently disabled-by-habit inside a printer
/// session (see `execute`'s `help` arm). Usable at any time, independent
/// of whether a `term_printer` session is currently open.
#[inline(never)]
fn print_command(info: &BootInfo, arguments: &[u8]) {
    const USAGE: &str = "Usage: print [diskN] NAME | print run CMD...";

    let (first, remainder) = take_word(arguments);
    if first.eq_ignore_ascii_case(b"run") {
        if remainder.is_empty() {
            println!("{USAGE}");
            return;
        }
        print_run(remainder, info);
        return;
    }

    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, trailing) = take_word(arguments);
    if name.is_empty() || !trailing.is_empty() {
        println!("{USAGE}");
        return;
    }
    print_file(info, device, name);
}

/// Reads a whole file through the shared tape staging buffer
/// (`pmem::TAPE_BASE`/`TAPE_LEN`, exactly like `copy_file` -- see that
/// function's doc comment for why sharing it with the tape subsystem is
/// safe) and queues its raw bytes onto the print relay via `console::
/// print_relay_push_raw`, bypassing VGA/serial entirely: a file being
/// printed is not also a transcript of anything typed, so there is
/// nothing to echo.
///
/// Nesting-aware, exactly like `print_run` (see its doc comment for why):
/// if a `term_printer` session's own relay is already active (this was
/// typed as a command from inside one), this only queues the file's bytes
/// and leaves enabling/draining/disabling to that outer session, so the
/// file's content lands on the same paper as everything else the session
/// prints instead of splitting into its own separate, premature print job.
/// Otherwise (typed at the ordinary local prompt via `print [diskN]
/// NAME`), it owns the whole enable-queue-drain-disable cycle itself.
fn print_file(info: &BootInfo, device: disk::Device, name: &[u8]) {
    let Some((printer_name, base)) = default_printer_port() else {
        println!("No LPT port was detected by the BIOS at boot; see 'printer'.");
        return;
    };
    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length > u32::MAX as u64 {
        println!("File is too large to print through the shared tape-buffer scratch area.");
        return;
    }
    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, device, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    invalidate_tape_after_scratch_use();

    let owns_relay = !console::print_relay_active();
    let printer_name = core::str::from_utf8(printer_name).unwrap_or("?");
    println!(
        "Printing {length} byte(s) from disk{} '{}' to {printer_name}...",
        device_number(device),
        core::str::from_utf8(name).unwrap_or("?")
    );
    if owns_relay {
        console::enable_print_relay();
    }
    console::print_relay_push_raw(buffer);
    if !owns_relay {
        // The outer `term_printer` session owns draining/disabling this
        // queue; just leave the bytes queued for it.
        return;
    }
    let printer = parallel::Printer::new(base);
    let finished = drain_print_relay(&printer);
    console::disable_print_relay();
    if finished {
        println!("Done.");
    } else {
        println!("Gave up printing '{}'.", core::str::from_utf8(name).unwrap_or("?"));
    }
}

/// `print run CMD...` runs `command` with the print-relay queue enabled
/// (so its ordinary console output is captured, exactly as
/// `term_printer`'s own committed commands are) and then drains that
/// queue to the printer -- but, unlike `term_printer`, never injects the
/// command line itself into the queue: this is meant purely as "print
/// what this command would have shown," not a transcript entry.
///
/// Temporarily clears `PRINTER_SESSION_ACTIVE` around the nested
/// `execute` call so `print run help` (the documented, deliberate
/// exception to `help`'s printer-session refusal -- see that arm's doc
/// comment) works even when typed *from inside* an active `term_printer`
/// session, not only from the ordinary local prompt; the previous value
/// is restored afterward regardless of which one that was.
///
/// Nesting-aware: `console::print_relay_active()` is checked *before* any
/// of the above, since a `term_printer` session's own committed-line flow
/// already enables the print relay and queues that line's transcript
/// before calling `execute` (which is what reaches this function for
/// `print run ...` typed inside a session). Calling `enable_print_relay`
/// again here would discard that not-yet-drained transcript (it resets
/// the queue), and draining/disabling here would end the outer session's
/// print job early, splitting one line's worth of output across two
/// separate physical print jobs instead of one. When nested, this instead
/// just runs `command` and leaves every bit of enabling, draining, and
/// disabling to the outer `term_printer` loop.
fn print_run(command: &[u8], info: &BootInfo) {
    let Some((printer_name, base)) = default_printer_port() else {
        println!("No LPT port was detected by the BIOS at boot; see 'printer'.");
        return;
    };
    let owns_relay = !console::print_relay_active();
    let printer_name = core::str::from_utf8(printer_name).unwrap_or("?");
    println!(
        "Running '{}' and printing its output to {printer_name}...",
        core::str::from_utf8(command).unwrap_or("?")
    );
    if owns_relay {
        console::enable_print_relay();
    }
    let was_active = unsafe { PRINTER_SESSION_ACTIVE };
    unsafe {
        PRINTER_SESSION_ACTIVE = false;
    }
    execute(command, info);
    unsafe {
        PRINTER_SESSION_ACTIVE = was_active;
    }
    if !owns_relay {
        return;
    }
    let printer = parallel::Printer::new(base);
    let finished = drain_print_relay(&printer);
    console::disable_print_relay();
    if finished {
        println!("Done.");
    } else {
        println!("Gave up printing the command's output.");
    }
}


/// A minimal raw serial terminal emulator: once entered, the chosen port
/// stops being available for anything else and instead becomes a
/// dedicated link to whatever is on the other end of the wire, byte for
/// byte, with no local line editing or echo -- the remote side is expected
/// to echo your keystrokes itself, exactly as a real terminal program
/// would.
///
/// With no arguments, `term` reuses the single COM1 port most real
/// machines actually have, at its fixed 115200 baud (see below). Because
/// this repurposes COM1, a serial-only session controlling fos through
/// that same port would lose its ability to type further commands once
/// `term` starts; local VGA + PS/2 keyboard input is unaffected and
/// remains the way to invoke `term` and to leave it again with F12 (or F4/F6/F7/F8/F10)
/// (chosen specifically because function keys sit outside the
/// Shift/AltGr layers that a Nordic keyboard puts most punctuation
/// behind).
///
/// `term COM2`/`COM3`/`COM4`/`PCI0`/`PCI1`/... [BAUD] uses an additional
/// port instead, if one was detected (see `serial`): COM2-4 via the BIOS
/// Data Area, PCI ports via PCI configuration-space enumeration
/// (`console::detect_pci_ports`). None of those are ever used by the
/// interactive shell, so -- unlike COM1, whose rate must stay fixed so it
/// keeps matching whatever terminal program is already attached to it --
/// their baud rate can be freely chosen; it must evenly divide 115200
/// (the UART's fixed clock) so the requested rate is reproduced exactly,
/// e.g. 115200, 57600, 38400, 19200, 9600, 4800, 2400, 1200.
///
/// `term net` is unrelated to any serial port: it waits for a remote
/// terminal client to say hello over Ethernet (see `term_net`), then
/// hands local keyboard/VGA control over entirely so the connected peer
/// drives this same live shell -- unlike `term COM1`/etc, which is a dumb
/// byte pipe with no shell involvement at all.
#[inline(never)]
fn term_command(info: &BootInfo, arguments: &[u8]) {
    const USAGE: &str =
        "Usage: term [COM1|COM2|COM3|COM4|PCI0|PCI1|...|printer|LPT1|...|net] [BAUD]";

    let (port_word, remainder) = take_word(arguments);
    let (baud_word, remainder) = take_word(remainder);
    if !remainder.is_empty() {
        println!("{USAGE}");
        return;
    }

    let baud = if baud_word.is_empty() {
        None
    } else {
        match parse_number(baud_word) {
            Some(baud) => Some(baud),
            None => {
                println!("Invalid baud rate.");
                return;
            }
        }
    };

    if port_word.eq_ignore_ascii_case(b"net") {
        if baud.is_some() {
            println!("{USAGE}");
            return;
        }
        term_net(info);
        return;
    }

    if port_word.eq_ignore_ascii_case(b"printer") {
        if baud.is_some() {
            println!("{USAGE}");
            return;
        }
        let ports = parallel::detect_ports();
        let Some(index) = ports.iter().position(Option::is_some) else {
            println!("No LPT port was detected by the BIOS at boot; see 'printer'.");
            return;
        };
        term_printer(info, PRINTER_PORT_NAMES[index], ports[index].unwrap());
        return;
    }

    if let Some(index) = PRINTER_PORT_NAMES
        .iter()
        .position(|name| name.eq_ignore_ascii_case(port_word))
    {
        if baud.is_some() {
            println!("{USAGE}");
            return;
        }
        let ports = parallel::detect_ports();
        let Some(base) = ports[index] else {
            let name = core::str::from_utf8(PRINTER_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected by the BIOS at boot; see 'printer'.");
            return;
        };
        term_printer(info, PRINTER_PORT_NAMES[index], base);
        return;
    }

    if port_word.is_empty() || SERIAL_PORT_NAMES[0].eq_ignore_ascii_case(port_word) {
        if baud.is_some() {
            println!("COM1's baud rate is fixed at 115200 (it is shared with the local shell");
            println!("console, so changing it mid-session would desync anyone attached to it).");
            println!("Use COM2, COM3, COM4, or a PCI port for a custom baud rate.");
            return;
        }
        println!("Entering raw terminal mode on COM1 at 115200 baud: your keystrokes are sent");
        println!("byte-for-byte, with no local echo or line editing -- the far end echoes them");
        println!("back. Press F12 (or F4/F6/F7/F8/F10) to return to the fos shell.");
        println!("If you are controlling fos over this same serial link, note that link");
        println!("is about to become the remote connection instead: use the VGA console");
        println!("to get back in that case.");
        term_loop(console::write_serial_byte, console::read_serial_byte);
        println!("\nLeft terminal mode; back at the fos shell.");
        return;
    }

    if let Some(index) = SERIAL_PORT_NAMES
        .iter()
        .position(|name| name.eq_ignore_ascii_case(port_word))
    {
        let ports = console::detect_ports();
        let Some(base) = ports[index] else {
            let name = core::str::from_utf8(SERIAL_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected by the BIOS at boot; see 'serial'.");
            return;
        };
        run_uart_term(SERIAL_PORT_NAMES[index], base, baud);
        return;
    }

    if let Some(index) = PCI_PORT_NAMES
        .iter()
        .position(|name| name.eq_ignore_ascii_case(port_word))
    {
        let (pci_ports, pci_count) = console::detect_pci_ports();
        if index >= pci_count {
            let name = core::str::from_utf8(PCI_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected via PCI enumeration; see 'serial'.");
            return;
        }
        run_uart_term(PCI_PORT_NAMES[index], pci_ports[index], baud);
        return;
    }

    println!("{USAGE}");
}

/// Shared by every non-COM1 port (COM2-4 and PCI-discovered ports): each
/// gets its own freshly configured `console::Uart` rather than the shared
/// console link, so it may use any baud rate the caller asks for.
fn run_uart_term(name_bytes: &[u8], base: u16, baud: Option<u32>) {
    let name = core::str::from_utf8(name_bytes).unwrap_or("?");
    let baud = baud.unwrap_or(console::UART_CLOCK);
    let uart = console::Uart::new(base);
    if !uart.configure(baud) {
        println!("Could not initialize {name} at {baud} baud (rate must evenly divide 115200,");
        println!("e.g. 115200, 57600, 38400, 19200, 9600, 4800, 2400, 1200).");
        return;
    }
    println!("Entering raw terminal mode on {name} at {baud} baud: your keystrokes are sent");
    println!("byte-for-byte, with no local echo or line editing -- the far end echoes them");
    println!("back. Press F12 (or F4/F6/F7/F8/F10) to return to the fos shell.");
    term_loop(|byte| uart.write_byte(byte), || uart.read_byte());
    println!("\nLeft terminal mode; back at the fos shell.");
}

/// Shared by every `term` port: forwards keystrokes via `write` until F12 (or F4/F6/F7/F8/F10)
/// is pressed, and shows bytes arriving via `read` locally (VGA only, so
/// they are never re-echoed back over the same wire they arrived on).
fn term_loop(write: impl Fn(u8), read: impl Fn() -> Option<u8>) {
    loop {
        match keyboard::poll_term_event() {
            Some(keyboard::KeyEvent::Escape) => break,
            Some(keyboard::KeyEvent::Char(byte)) => write(byte),
            // This raw byte pipe has no line concept for F2 to discard;
            // only `term_printer`'s own typing loop gives it meaning.
            Some(keyboard::KeyEvent::KillLine) | None => {}
        }
        if let Some(byte) = read() {
            console::write_vga_only(&[byte]);
        }
    }
}

/// A full remote shell over Ethernet: unlike `term`'s raw byte pipe, this
/// hands the actual local shell -- prompt, line editing, and every
/// existing command (`nic`, `chat`, `tape`, ...) -- over to whichever
/// remote peer connects, so a client only needs to speak the small
/// `TERM_TAG_*` framing below, not reimplement any command logic.
///
/// Waits (F12 (or F4/F6/F7/F8/F10) to give up, same convention as `nic discover`/`chat`/`term`)
/// for a `TERM_TAG_HELLO` from any peer, then locks onto that peer's MAC
/// for the rest of the session -- a second `HELLO` from someone else while
/// already connected is ignored, since only one remote client can drive
/// this single-threaded shell at a time. Once connected:
///
/// - Everything the shell would normally print to VGA/COM1 (prompts,
///   echoed keystrokes, command output) is instead captured by
///   `console::enable_relay` and forwarded as `TERM_TAG_DATA` frames, so
///   the remote peer sees exactly what a local operator would.
/// - Incoming `TERM_TAG_DATA` payload bytes are fed one at a time into a
///   fresh `LineEditor`, exactly as if they had been typed locally,
///   including running `execute` on Enter.
/// - Local VGA/PS/2 input is *not* read at all while connected (this
///   session is exclusive, not mirrored): the only way to interrupt it
///   locally is F12 (or F4/F6/F7/F8/F10), which sends `TERM_TAG_BYE` to the peer and
///   returns control to the local shell for good. If the *peer* ends the
///   session first (its own `TERM_TAG_BYE`, or it simply vanishes and a
///   later `HELLO` from someone else arrives), this does not return either
///   -- it goes back to waiting for a new client instead, so the only way
///   to actually leave `term net` and reach the local shell is the local
///   escape key, never a remote disconnect.
///
/// Like `tape_load_net`, this answers any `CHAT_TAG_DISCOVER_REQUEST` it
/// sees the whole time it runs -- both before a client has connected and
/// during an active session -- so simply leaving `term net` running is
/// enough to make this host discoverable, exactly as `chat`/`tape load
/// net` already do. While waiting, probes are answered on every
/// initialized NIC, not only the active terminal NIC.
///
/// `shell::run` calls this once automatically at every boot, before the
/// ordinary interactive prompt, so every fos machine is reachable over
/// `term net` by default with no manual step -- this matters most for a
/// machine with no attached screen/keyboard. A local operator watching a
/// real screen presses F12 (or F4/F6/F7/F8/F10) to skip the wait and get
/// the interactive prompt immediately; after that local escape, one minute
/// without local keyboard or serial activity re-enters the wait. There is
/// deliberately no serial equivalent for the escape key, since reading one
/// byte here to mean "give up" would silently eat the first character of
/// whatever a serial operator was about to type.
fn term_net(info: &BootInfo) {
    let Some(nic) = nic_or_report() else { return };
    let source = nic.mac_address();
    let active_index = nic::active_index();
    let interface_count = nic::count();
    println!("Waiting for a remote terminal client to connect; press F12");
    println!("(or F4/F6/F7/F8/F10) to give up.");

    let mut recv_buffer = [0u8; nic::MAX_FRAME_LEN];
    loop {
        let peer = loop {
            if let Some(keyboard::KeyEvent::Escape) = keyboard::poll_term_event() {
                println!("Gave up waiting for a remote terminal client.");
                return;
            }
            let mut connected_peer = None;
            for index in 0..interface_count {
                let Some(interface) = nic::get_at(index) else {
                    continue;
                };
                let Some(received) = interface.receive(&mut recv_buffer) else {
                    continue;
                };
                let frame = &recv_buffer[..received];
                if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
                    continue;
                }
                let peer_mac: [u8; 6] = frame[6..12].try_into().unwrap();
                match frame[14] {
                    TERM_TAG_HELLO if index == active_index => {
                        connected_peer = Some(peer_mac);
                        break;
                    }
                    CHAT_TAG_DISCOVER_REQUEST => {
                        let interface_mac = interface.mac_address();
                        remember_peer(peer_mac);
                        send_chat_control(
                            interface,
                            &peer_mac,
                            &interface_mac,
                            CHAT_TAG_DISCOVER_REPLY,
                        );
                        println!(
                            "\n(replied to a discovery probe from {} on NIC {})",
                            Mac(&peer_mac),
                            index + 1
                        );
                    }
                    _ => {}
                }
            }
            if let Some(peer) = connected_peer {
                break peer;
            }
        };

        println!("Remote terminal connected from {}. Local input is disabled;", Mac(&peer));
        println!("F12 (or F4/F6/F7/F8/F10) disconnects it.");
        console::enable_relay();
        // Lets `poll_remote_tape_abort` recognize this peer's own frames
        // while a nested blocking command (like `tape_run`) has this
        // loop's own `nic.receive` paused -- cleared again below once
        // this session ends, so a tape started from the local shell (no
        // active peer) never mistakes leftover state for one.
        unsafe {
            ACTIVE_TERM_PEER = Some(peer);
            ACTIVE_TERM_SOURCE = Some(source);
            PENDING_REMOTE_BYE = false;
            TERM_DEFERRED_INPUT_HEAD = 0;
            TERM_DEFERRED_INPUT_TAIL = 0;
            TERM_DEFERRED_INPUT_COUNT = 0;
            TERM_DEFERRED_INPUT_OVERFLOWED = false;
        }
        let mut editor = LineEditor::new();
        let mut relay_chunk = [0u8; TERM_DATA_CAPACITY];
        prompt();

        // Tracks *why* the session below ended: only a local escape key
        // press should ever fall through to the local shell (see this
        // function's own doc comment) -- a remote `TERM_TAG_BYE` instead
        // loops back to waiting for a new client.
        let mut local_escape = false;
        'session: loop {
            // Set by `poll_remote_tape_abort` if a nested tape's own
            // network poll already consumed this peer's `TERM_TAG_BYE`
            // while this loop's own `nic.receive` below was paused: the
            // real disconnect already happened, so there is no second
            // `TERM_TAG_BYE` left to receive here -- this is exactly as
            // if the `TERM_TAG_BYE` arm just below had matched.
            if unsafe { PENDING_REMOTE_BYE } {
                break 'session;
            }

            drain_deferred_term_input(&mut editor, info);

            let queued = console::peek_relay(&mut relay_chunk);
            if queued > 0 && send_term_data(nic, &peer, &source, &relay_chunk[..queued]) {
                console::discard_relay(queued);
            }

            if let Some(keyboard::KeyEvent::Escape) = keyboard::poll_term_event() {
                send_term_control(nic, &peer, &source, TERM_TAG_BYE);
                local_escape = true;
                break 'session;
            }

            if let Some(received) = nic.receive(&mut recv_buffer) {
                let frame = &recv_buffer[..received];
                if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
                    continue;
                }
                let frame_source: [u8; 6] = frame[6..12].try_into().unwrap();
                if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
                    remember_peer(frame_source);
                    send_chat_control(nic, &frame_source, &source, CHAT_TAG_DISCOVER_REPLY);
                    continue;
                }
                if frame_source != peer {
                    continue;
                }
                match frame[14] {
                    TERM_TAG_BYE => break 'session,
                    TERM_TAG_DATA => {
                        let payload = &frame[15..];
                        let end = payload
                            .iter()
                            .rposition(|&byte| byte != 0)
                            .map_or(0, |index| index + 1);
                        for &byte in &payload[..end] {
                            editor.feed(byte, info);
                        }
                    }
                    _ => {}
                }
            }
        }

        console::disable_relay();
        unsafe {
            ACTIVE_TERM_PEER = None;
            ACTIVE_TERM_SOURCE = None;
        }
        if local_escape {
            println!("\nRemote terminal disconnected; back at the local fos shell.");
            return;
        }
        println!(
            "\nRemote terminal disconnected; waiting for a new one to connect (F12 (or F4/F6/F7/F8/F10) to stop and use the local shell)."
        );
    }
}

/// The peer and source MAC of the `term_net` session currently driving
/// this shell, if any -- `None` whenever no remote terminal is connected
/// (including the entire time this function is running from a local
/// keyboard). Set and cleared by `term_net` itself around its per-peer
/// session loop; read by `poll_remote_tape_abort` so a nested blocking
/// command like `tape_run`, called deep inside that loop's own
/// `editor.feed`/`execute` call chain with no arguments of its own to
/// thread this through, can still find out who to listen for.
static mut ACTIVE_TERM_PEER: Option<[u8; 6]> = None;
static mut ACTIVE_TERM_SOURCE: Option<[u8; 6]> = None;

/// Set for as long as `term_printer`'s interactive session (or a
/// deliberately nested `execute` call it makes for a committed line) is
/// running, so `execute`'s own `help` arm can refuse to run (see its doc
/// comment) rather than flooding the connected printer with its listing.
/// Never set by `print_command`'s one-shot `print run CMD`, which is the
/// intended way to print `help` deliberately, once.
static mut PRINTER_SESSION_ACTIVE: bool = false;

/// Set by `poll_remote_tape_abort` when it consumes the connected peer's
/// own `TERM_TAG_BYE` while `term_net`'s own session loop was paused
/// inside a nested blocking command; checked (and reset by starting a
/// new session) at the top of that loop so a disconnect noticed this way
/// is not lost, since the frame carrying it can only ever be received
/// once.
static mut PENDING_REMOTE_BYE: bool = false;

/// `term net`'s editor is suspended while a nested command owns NIC polling.
/// Retain complete data frames from its active peer here so commands sent
/// during (for example) `tape load net` run after that command returns.
/// Four protocol-sized frames cover the expected short command burst without
/// adding a heap or letting a stalled client consume unbounded kernel memory.
const TERM_DEFERRED_INPUT_FRAME_COUNT: usize = 4;
static mut TERM_DEFERRED_INPUT_FRAMES: [[u8; TERM_DATA_CAPACITY];
    TERM_DEFERRED_INPUT_FRAME_COUNT] = [[0; TERM_DATA_CAPACITY];
    TERM_DEFERRED_INPUT_FRAME_COUNT];
static mut TERM_DEFERRED_INPUT_LENS: [usize; TERM_DEFERRED_INPUT_FRAME_COUNT] =
    [0; TERM_DEFERRED_INPUT_FRAME_COUNT];
static mut TERM_DEFERRED_INPUT_HEAD: usize = 0;
static mut TERM_DEFERRED_INPUT_TAIL: usize = 0;
static mut TERM_DEFERRED_INPUT_COUNT: usize = 0;
static mut TERM_DEFERRED_INPUT_OVERFLOWED: bool = false;

/// Saves one active-peer terminal frame for the suspended `LineEditor`.
/// Oversized frames or a full queue drop the whole newest frame (never a
/// partial command) and arrange one explicit diagnostic once the editor
/// resumes. `TERM_TAG_BYE` remains a session event, never editor input.
fn defer_active_term_input(frame: &[u8]) -> bool {
    if !console::relay_enabled()
        || frame.len() < 15
        || frame[12..14] != CHAT_ETHERTYPE
        || frame[6..12] != unsafe { ACTIVE_TERM_PEER }.unwrap_or([0; 6])
    {
        return false;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            true
        }
        TERM_TAG_DATA => {
            let payload = &frame[15..];
            let length = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            unsafe {
                if length > TERM_DATA_CAPACITY
                    || TERM_DEFERRED_INPUT_COUNT == TERM_DEFERRED_INPUT_FRAME_COUNT
                {
                    TERM_DEFERRED_INPUT_OVERFLOWED = true;
                } else {
                    TERM_DEFERRED_INPUT_FRAMES[TERM_DEFERRED_INPUT_TAIL][..length]
                        .copy_from_slice(&payload[..length]);
                    TERM_DEFERRED_INPUT_LENS[TERM_DEFERRED_INPUT_TAIL] = length;
                    TERM_DEFERRED_INPUT_TAIL =
                        (TERM_DEFERRED_INPUT_TAIL + 1) % TERM_DEFERRED_INPUT_FRAME_COUNT;
                    TERM_DEFERRED_INPUT_COUNT += 1;
                }
            }
            true
        }
        _ => false,
    }
}

/// Replays deferred complete terminal frames only after the blocking command
/// that consumed them has returned to `term_net`.
fn drain_deferred_term_input(editor: &mut LineEditor, info: &BootInfo) {
    if unsafe { TERM_DEFERRED_INPUT_OVERFLOWED } {
        unsafe { TERM_DEFERRED_INPUT_OVERFLOWED = false };
        println!("\nRemote terminal input overflowed while a blocking command");
        println!("owned the NIC; dropped newest frame(s).");
    }
    loop {
        let mut frame = [0u8; TERM_DATA_CAPACITY];
        let length = unsafe {
            if TERM_DEFERRED_INPUT_COUNT == 0 {
                return;
            }
            let head = TERM_DEFERRED_INPUT_HEAD;
            let length = TERM_DEFERRED_INPUT_LENS[head];
            frame[..length].copy_from_slice(&TERM_DEFERRED_INPUT_FRAMES[head][..length]);
            TERM_DEFERRED_INPUT_LENS[head] = 0;
            TERM_DEFERRED_INPUT_HEAD = (head + 1) % TERM_DEFERRED_INPUT_FRAME_COUNT;
            TERM_DEFERRED_INPUT_COUNT -= 1;
            length
        };
        for &byte in &frame[..length] {
            editor.feed(byte, info);
        }
    }
}

/// The byte a remote `term net` peer can send inside a `TERM_TAG_DATA`
/// frame to ask a currently running blocking command (right now, only
/// `tape_run`'s loop) to stop -- the network equivalent of the local
/// "give up" keys (F12/F4/F6/F7/F8/F10) `keyboard::ESCAPE_SCANCODES`
/// recognizes. Plain ASCII Escape was picked because it needs no
/// translation on the client side: `tools/term_net.py --interactive`
/// already forwards a raw Escape keypress as exactly this byte, with no
/// local echo or interpretation of its own.
const REMOTE_ABORT_BYTE: u8 = 0x1b;

/// Copies whatever console output is queued in `console`'s relay buffer
/// and forwards it to `peer` as one `TERM_TAG_DATA` frame -- the same
/// "send, then discard" step
/// `term_net`'s own session loop performs every iteration, factored out
/// so `poll_remote_tape_abort`/`poll_remote_tape_input` can perform it
/// too. Without this, a nested blocking command's own `putc` output
/// (which still reaches `console::write_bytes`/`relay_push` exactly like
/// any other console output) would only ever reach the relay queue, not
/// the wire, until `term_net`'s own outer loop resumed -- fine for a
/// one-shot tape whose whole output arrives in one burst once it returns
/// (see AGENTS.md's `tools/test_term_net_tape_e2e.py` notes), but wrong
/// for a tape that loops for a long time printing incrementally (an
/// interactive REPL like `tapes/calc`, or a future long-running
/// `should_abort` tape that prints progress) -- a remote operator would
/// otherwise see nothing at all until the whole thing finally exits.
/// Calling this from both polling helpers below means any `read_key`- or
/// `should_abort`-based tape gets live output relaying for free, with no
/// ABI change and no per-tape opt-in required.
fn relay_pending_tape_output(nic: &nic::Nic, peer: &[u8; 6], source: &[u8; 6]) {
    let mut chunk = [0u8; TERM_DATA_CAPACITY];
    let queued = console::peek_relay(&mut chunk);
    if queued > 0 && send_term_data(nic, peer, source, &chunk[..queued]) {
        console::discard_relay(queued);
    }
}

/// Polls the network card once for a frame from the currently connected
/// `term_net` peer, if any, on behalf of a nested blocking command (see
/// `tape_run`) whose own loop has `term_net`'s *own* session loop --
/// including its `nic.receive` -- paused for as long as it runs. Mirrors
/// that loop's own frame handling closely enough to stay a reasonable
/// network citizen even mid-tape (still answering discovery probes from
/// someone else, still noticing this peer hanging up, still relaying
/// this tape's own printed output live via `relay_pending_tape_output`).
/// Frames that belong to the tape rather than the terminal session are
/// preserved in the deferred raw-frame queue instead of being lost here.
///
/// Returns whether the caller's blocking command should stop: either the
/// peer explicitly asked to (a `TERM_TAG_DATA` frame containing
/// `REMOTE_ABORT_BYTE` anywhere in its payload -- the whole frame is
/// simply dropped rather than fed to the line editor, the same way a
/// local escape key press is consumed rather than typed as a literal
/// character) or the peer hung up entirely (`TERM_TAG_BYE`, which also
/// records `PENDING_REMOTE_BYE` since this call is the only place that
/// frame will ever be received).
///
/// Does nothing but check one flag (and so costs a single branch) unless
/// a `term_net` relay session is actually active: a tape started from
/// the local shell has `console::relay_enabled()` false the entire time,
/// so it never touches the network card at all.
fn poll_remote_tape_abort() -> bool {
    if !console::relay_enabled() {
        return false;
    }
    let (Some(peer), Some(source)) = (unsafe { ACTIVE_TERM_PEER }, unsafe { ACTIVE_TERM_SOURCE })
    else {
        return false;
    };
    let Ok(nic) = nic::get() else { return false };
    relay_pending_tape_output(nic, &peer, &source);
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let Some(received) = nic.receive(&mut buffer) else {
        return false;
    };
    let frame = &buffer[..received];
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        tape_network_frame_push(frame);
        return false;
    }
    let frame_source: [u8; 6] = frame[6..12].try_into().unwrap();
    if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
        remember_peer(frame_source);
        send_chat_control(nic, &frame_source, &source, CHAT_TAG_DISCOVER_REPLY);
        tape_network_frame_push(frame);
        return false;
    }
    if frame_source != peer {
        tape_network_frame_push(frame);
        return false;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            true
        }
        TERM_TAG_DATA => {
            if frame[15..].contains(&REMOTE_ABORT_BYTE) {
                true
            } else {
                defer_active_term_input(frame);
                false
            }
        }
        _ => {
            tape_network_frame_push(frame);
            false
        }
    }
}

/// Sends `data` as one `TERM_TAG_DATA` frame to `destination`.
///
/// The caller retains the corresponding relay bytes when this returns
/// `false`, allowing a later foreground iteration to retry them in order.
fn send_term_data(
    nic: &nic::Nic,
    destination: &[u8; 6],
    source: &[u8; 6],
    data: &[u8],
) -> bool {
    let mut frame = [0u8; 15 + TERM_DATA_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = TERM_TAG_DATA;
    frame[15..15 + data.len()].copy_from_slice(data);
    nic.send(&frame[..15 + data.len()])
}

/// Sends a zero-length `CHAT_ETHERTYPE` control frame with a `TERM_TAG_*`
/// tag (`HELLO` or `BYE`), mirroring `send_chat_control`.
fn send_term_control(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], tag: u8) {
    let mut frame = [0u8; 15];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    nic.send(&frame);
}

/// reserved by IEEE 802 for exactly this purpose: local, non-routable
/// protocol experiments that will never collide with a real assigned
/// ethertype), used to mark `nic send`'s frames as fos's own test traffic
/// rather than anything meaningful to a real network.
const TEST_ETHERTYPE: [u8; 2] = [0x88, 0xb5];
const TEST_PAYLOAD: &[u8] = b"fos raw ethernet test frame";

/// `nic` (no arguments) reports the active interface; `nic list` shows every
/// initialized interface; and `nic use N` selects one by its 1-based list
/// number. `nic send` transmits one broadcast test frame (see
/// `TEST_ETHERTYPE`); `nic recv` polls briefly for one incoming frame and
/// hex-dumps it if one arrives; `nic sniff` runs a continuous
/// promiscuous-mode capture, printing every frame seen until F12 (or
/// F4/F6/F7/F8/F10) stops it; `nic discover` broadcasts a `chat` discovery
/// probe through every initialized interface and lists whoever answers;
/// `nic peers` shows the peer list discovered so far without sending
/// anything. These shell subcommands intentionally stay at the raw-Ethernet
/// layer; ARP/IPv4/UDP/TCP/DHCP live in networking tapes and the shared
/// `tapes/tape-net` library instead.
#[inline(never)]
fn nic_command(arguments: &[u8]) {
    let (subcommand, remainder) = take_word(arguments);
    match subcommand {
        b"" if remainder.is_empty() => nic_status(),
        b"list" if remainder.is_empty() => nic_list(),
        b"use" => nic_use(remainder),
        b"send" if remainder.is_empty() => nic_send(),
        b"recv" if remainder.is_empty() => nic_recv(),
        b"sniff" if remainder.is_empty() => nic_sniff(),
        b"discover" if remainder.is_empty() => nic_discover(),
        b"peers" if remainder.is_empty() => print_peers(),
        b"watchdog" if remainder.is_empty() => nic_watchdog(),
        _ => println!("Usage: nic [list|use N|send|recv|sniff|discover|peers|watchdog]"),
    }
}

fn nic_status() {
    match nic::get() {
        Ok(nic) => {
            report_nic_watchdog(nic.watchdog());
            let mac = nic.mac_address();
            println!(
                "{} found: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                nic.driver_name(),
                mac[0],
                mac[1],
                mac[2],
                mac[3],
                mac[4],
                mac[5]
            );
            if let Some(link_up) = nic.link_up() {
                println!(
                    "3Com media link: {} (raw link-beat status; check cable/switch if down).",
                    if link_up { "up" } else { "down" }
                );
            }
            print_nic_interrupt_status(nic);
            print_nic_health(nic);
            let count = nic::count();
            if count > 1 {
                println!(
                    "Active interface: {} of {}. Use 'nic list' or 'nic use N' to select another.",
                    nic::active_index() + 1,
                    count
                );
            }
        }
        Err(nic::Status::NotPresent) => {
            println!("No supported Ethernet controller found on the PCI bus.")
        }
        Err(nic::Status::InitFailed(error)) => {
            println!("A supported Ethernet controller was found but could not");
            println!("be initialized: {error}");
        }
    }
}

fn nic_list() {
    let count = nic::count();
    if count == 0 {
        nic_status();
        return;
    }
    for index in 0..count {
        let Some(nic) = nic::get_at(index) else {
            continue;
        };
        let mac = nic.mac_address();
        println!(
            "{}NIC {}: {} MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            if index == nic::active_index() {
                '*'
            } else {
                ' '
            },
            index + 1,
            nic.driver_name(),
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
        report_nic_watchdog(nic.watchdog());
        print_nic_interrupt_status(nic);
        print_nic_health(nic);
    }
}

fn nic_watchdog() {
    match nic::watchdog() {
        Ok(result) => report_nic_watchdog(result),
        Err(nic::Status::NotPresent) => {
            println!("No supported Ethernet controller found on the PCI bus.")
        }

        Err(nic::Status::InitFailed(error)) => {
            println!("A supported Ethernet controller was found but could not");
            println!("be initialized: {error}");
        }
    }
}

fn report_nic_watchdog(result: nic::WatchdogResult) {
    match result {
        nic::WatchdogResult::Healthy => {}
        nic::WatchdogResult::Recovered => {
            println!("NIC watchdog: recovered a documented adapter fault in");
            println!("foreground; polling remains available.");
        }
        nic::WatchdogResult::Failed => {
            println!("NIC watchdog: bounded adapter reset/rebuild failed; polling");
            println!("remains available, but this card may need a reset.");
        }
    }
}

fn print_nic_health(nic: &nic::Nic) {
    let health = nic.health();
    println!(
        "NIC health: Rx {} frame(s), Tx {} frame(s), {} recovery/recoveries,",
        health.rx_progress, health.tx_progress, health.recoveries,
    );
    println!(
        "{} failed; {}.",
        health.recovery_failures,
        if health.recovery_pending {
            "foreground recovery pending"
        } else {
            "no recovery pending"
        }
    );
}

fn print_nic_interrupt_status(nic: &nic::Nic) {
    let stats = nic.interrupt_stats();
    match stats.irq {
        Some(irq) if interrupts::pci_irq_registered(irq) => {
            println!(
                "NIC IRQ{irq}: {} notification(s), {} device error notification(s),",
                stats.count, stats.errors,
            );
            println!(
                "foreground work {}.",
                if stats.pending { "pending" } else { "not pending" }
            );
        }
        Some(irq) => println!(
            "NIC IRQ{irq}: legacy line is unavailable or reserved; polling remains active."
        ),
        None => println!("NIC has no firmware-assigned legacy IRQ; polling remains active."),
    }
}

fn nic_use(arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(index) = parse_number(value) else {
        println!("Usage: nic use N (see 'nic list'; numbering starts at 1)");
        return;
    };
    if !remainder.is_empty() || index == 0 || !nic::set_active(index as usize - 1) {
        println!("No such NIC. Use 'nic list' to see available interfaces.");
        return;
    }
    let nic = nic::get().unwrap();
    println!(
        "Active interface is now {} {}.",
        nic::active_index() + 1,
        nic.driver_name()
    );
}

fn nic_send() {
    let Some(nic) = nic_or_report() else { return };
    let source = nic.mac_address();
    let mut frame = [0u8; 14 + TEST_PAYLOAD.len()];
    frame[0..6].fill(0xff); // Broadcast destination.
    frame[6..12].copy_from_slice(&source);
    frame[12..14].copy_from_slice(&TEST_ETHERTYPE);
    frame[14..].copy_from_slice(TEST_PAYLOAD);

    if nic.send(&frame) {
        println!(
            "Sent a {}-byte broadcast test frame (ethertype 0x{:02x}{:02x}).",
            frame.len(),
            TEST_ETHERTYPE[0],
            TEST_ETHERTYPE[1]
        );
    } else {
        println!("Transmit failed (card reported an error, or timed out).");
    }
}

/// `term printer`/`term LPTn`: an exclusive terminal mode that runs the
/// live shell against a physical printer instead of a screen or a network
/// peer. Unlike `term_net`'s `LineEditor` (which echoes and executes each
/// byte as it arrives), a line here is buffered locally and edited purely
/// on screen until Enter commits it -- nothing reaches the printer for a
/// line discarded with F2 (`KeyEvent::KillLine`), since ink cannot be
/// erased the way a screen or a network peer's own line editor can.
///
/// Once a line commits: the typed command (echoed as `"fos> " + line`,
/// exactly matching the local prompt) and then that command's own output
/// are both queued via the print-relay mechanism (`console::
/// enable_print_relay`/`print_relay_push_raw`, tapped automatically by
/// every `println!`/`print!` call through `console::relay_push` -- see
/// its own doc comment) and drained to the printer by `drain_print_relay`,
/// which pauses and retries indefinitely on a fault (paper out, offline,
/// a latched error, or an unexpectedly long busy spell) rather than
/// dropping bytes, until it clears or the operator gives up with F12.
///
/// `help` refuses to run while `PRINTER_SESSION_ACTIVE` is set (see its
/// arm in `execute`), to avoid burning a whole sheet of paper by habit;
/// `print run help` still works, as a deliberate one-shot exception.
#[inline(never)]
fn term_printer(info: &BootInfo, name: &[u8], base: u16) {
    let name = core::str::from_utf8(name).unwrap_or("?");
    println!("Entering printer terminal mode on {name}. A typed line is only shown on");
    println!("screen and sent to the printer once you press Enter; F2 discards it first.");
    println!("Backspace edits the line normally and is never itself sent to the printer.");
    println!("Commands run here print their own output too; 'help' is disabled here --");
    println!("run 'print run help' once for a paper copy instead. Press F12 (or");
    println!("F4/F6/F7/F8/F10) to leave.");

    let printer = parallel::Printer::new(base);
    let mut buffer = [0u8; LINE_CAPACITY];
    let mut length = 0usize;
    prompt();
    loop {
        match keyboard::poll_term_event() {
            Some(keyboard::KeyEvent::Escape) => {
                println!();
                println!("Leaving printer terminal mode; back at the fos shell.");
                return;
            }
            Some(keyboard::KeyEvent::KillLine) => {
                while length > 0 {
                    length -= 1;
                    console::erase_last_character();
                }
            }
            Some(keyboard::KeyEvent::Char(b'\r' | b'\n')) => {
                println!();
                unsafe {
                    PRINTER_SESSION_ACTIVE = true;
                }
                console::enable_print_relay();
                console::print_relay_push_raw(b"fos> ");
                console::print_relay_push_raw(&buffer[..length]);
                console::print_relay_push_raw(b"\r\n");
                execute(&buffer[..length], info);
                let finished = drain_print_relay(&printer);
                console::disable_print_relay();
                unsafe {
                    PRINTER_SESSION_ACTIVE = false;
                }
                length = 0;
                if !finished {
                    println!("Leaving printer terminal mode; back at the fos shell.");
                    return;
                }
                prompt();
            }
            Some(keyboard::KeyEvent::Char(8 | 127)) if length > 0 => {
                length -= 1;
                console::erase_last_character();
            }
            Some(keyboard::KeyEvent::Char(byte @ 0x20..=0x7e)) if length < LINE_CAPACITY => {
                buffer[length] = byte;
                length += 1;
                print!("{}", byte as char);
            }
            Some(keyboard::KeyEvent::Char(_)) | None => {}
        }
    }
}

/// Shared by every `nic` subcommand that needs the card itself (`send`,
/// `recv`, `sniff`): reports either failure case the same way `nic`
/// (status) does, and returns `None` so callers can just early-return.
fn nic_or_report() -> Option<&'static nic::Nic> {
    match nic::get() {
        Ok(nic) => {
            report_nic_watchdog(nic.watchdog());
            Some(nic)
        }
        Err(nic::Status::NotPresent) => {
            println!("No supported Ethernet controller found on the PCI bus.");
            None
        }
        Err(nic::Status::InitFailed(error)) => {
            println!("A supported Ethernet controller was found but could not");
            println!("be initialized: {error}");
            None
        }
    }
}

fn nic_recv() {
    let Some(nic) = nic_or_report() else { return };
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut attempts = 20_000_000u32;
    loop {
        if let Some(length) = nic.receive(&mut buffer) {
            let frame = &buffer[..length];
            if length >= 14 {
                println!(
                    "Received {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    frame[0],
                    frame[1],
                    frame[2],
                    frame[3],
                    frame[4],
                    frame[5],
                    frame[6],
                    frame[7],
                    frame[8],
                    frame[9],
                    frame[10],
                    frame[11],
                    frame[12],
                    frame[13]
                );
            } else {
                println!("Received {length} bytes (too short to hold an Ethernet header).");
            }
            dump_bytes(0, frame);
            return;
        }
        attempts -= 1;
        if attempts == 0 {
            println!("No frame received (timed out waiting).");
            return;
        }
    }
}

/// Puts the card into promiscuous mode (`Rtl8139::set_promiscuous`) and
/// prints every frame it captures -- addressed to this card or not --
/// until F12 (or F4/F6/F7/F8/F10) stops the capture, the same escape key `term` uses and for
/// the same reason (layout-independent, unlike a Shift/AltGr combination
/// on a Nordic keyboard). Promiscuous mode is always turned back off
/// before returning, whether the capture ran to completion or not, so a
/// later `nic send`/`nic recv` is not left seeing unrelated traffic.
fn nic_sniff() {
    let Some(nic) = nic_or_report() else { return };

    println!("Enabling promiscuous mode: the card will now hand over every frame it sees");
    println!("on the wire, not just frames addressed to it (unicast/multicast/broadcast).");
    println!("Press F12 (or F4/F6/F7/F8/F10) to stop capturing and return to the fos shell.");
    nic.set_promiscuous(true);

    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut captured = 0u32;
    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            break;
        }
        if let Some(length) = nic.receive(&mut buffer) {
            captured += 1;
            let frame = &buffer[..length];
            if length >= 14 {
                println!(
                    "#{captured} {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    frame[0],
                    frame[1],
                    frame[2],
                    frame[3],
                    frame[4],
                    frame[5],
                    frame[6],
                    frame[7],
                    frame[8],
                    frame[9],
                    frame[10],
                    frame[11],
                    frame[12],
                    frame[13]
                );
            } else {
                println!("#{captured} {length} bytes (too short to hold an Ethernet header).");
            }
            dump_bytes(0, frame);
        }
    }

    nic.set_promiscuous(false);
    println!("\nPromiscuous mode disabled; captured {captured} frame(s). Back at the fos shell.");
}

/// A dedicated ethertype for `chat` and its discovery probes, distinct from
/// `TEST_ETHERTYPE`, so a `nic sniff` capture (or a stray `nic recv`) never
/// mistakes fos's own chat traffic for `nic send`'s test frames or vice
/// versa. `0x88b5`/`0x88b6` are the two ethertypes IEEE 802 reserves
/// together for exactly this kind of local, non-routable protocol
/// experiment.
const CHAT_ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const CHAT_TEXT_CAPACITY: usize = 200;
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

/// Every `CHAT_ETHERTYPE` frame carries a one-byte message tag right after
/// the Ethernet header, ahead of any per-message data, so one ethertype can
/// carry both ordinary chat lines and the small discovery exchange below
/// without inventing a second ethertype fos has no IEEE reservation for.
const CHAT_TAG_TEXT: u8 = 0x01;
const CHAT_TAG_DISCOVER_REQUEST: u8 = 0x02;
const CHAT_TAG_DISCOVER_REPLY: u8 = 0x03;

/// `tape load`/`tape send`'s wire protocol, sharing `CHAT_ETHERTYPE` rather
/// than inventing a third reserved ethertype: a paper-tape-style transfer
/// is `TAPE_TAG_START` (payload: the transfer's total length, 4 bytes
/// little-endian) once, then any number of `TAPE_TAG_DATA` frames (payload:
/// a 4-byte little-endian sequence number, then up to `TAPE_CHUNK_LEN` raw
/// bytes), then one `TAPE_TAG_STOP` (no payload) to close it.
const TAPE_TAG_START: u8 = 0x10;
const TAPE_TAG_DATA: u8 = 0x11;
const TAPE_TAG_STOP: u8 = 0x12;

/// Data bytes carried in one `TAPE_TAG_DATA` frame: comfortably under
/// `nic::MAX_FRAME_LEN` once the Ethernet header (14 bytes), tag
/// (1 byte), and sequence number (4 bytes) are accounted for, and a round
/// enough number to reason about by hand.
const TAPE_CHUNK_LEN: usize = 1024;

/// How often (in PIT ticks, 100/second -- see `interrupts::ticks`)
/// `tape_load_net` prints a progress line while a transfer is in flight.
const TAPE_STATUS_TICKS: u64 = 200; // ~2 seconds.

/// Spacing used by the in-kernel `tape send` producer. The receiver is a
/// polling-only NIC driver with a finite ring, so waiting one PIT tick
/// between frames keeps a real RTL8139 from overrunning that ring in the
/// same way `tools/send_tape.py --delay 0.003` does. This is deliberately
/// conservative because the PIT is only 100 Hz; a future acknowledged
/// transfer protocol could use a tighter, receiver-driven window instead.
const TAPE_SEND_PACE_TICKS: u64 = 1; // ~10 ms.

/// `term net`'s wire protocol, sharing `CHAT_ETHERTYPE` rather than
/// inventing a fourth reserved ethertype: a client says hello with
/// `TERM_TAG_HELLO` (no payload) to open a session, either side then sends
/// any number of `TERM_TAG_DATA` frames (payload: raw bytes, unlike
/// `tape`'s chunks there is no sequence number -- a dropped frame here is
/// just like a dropped keystroke or a torn screen update, not corrupted
/// data, so it is not worth the extra bookkeeping), and either side may
/// close the session with `TERM_TAG_BYE` (no payload).
const TERM_TAG_HELLO: u8 = 0x20;
const TERM_TAG_DATA: u8 = 0x21;
const TERM_TAG_BYE: u8 = 0x22;

/// Data bytes carried in one `TERM_TAG_DATA` frame in either direction:
/// generous for a burst of shell output (a whole `println!` line or two)
/// while staying comfortably under `nic::MAX_FRAME_LEN` once the
/// Ethernet header (14 bytes) and tag (1 byte) are accounted for.
const TERM_DATA_CAPACITY: usize = 512;

const MAX_PEERS: usize = 16;

/// Every MAC address any `nic discover` probe or `chat` auto-reply has ever
/// seen on this wire, in first-seen order; the shell's only address book
/// (see `remember_peer`/`peer_by_index`/`print_peers`). A plain array is
/// enough here, matching this project's general preference for small fixed
/// tables over dynamic collections (there is no heap in this kernel).
static mut PEERS: [[u8; 6]; MAX_PEERS] = [[0; 6]; MAX_PEERS];
static mut PEER_COUNT: usize = 0;

/// Records `mac` in the peer table if it is not already there, silently
/// dropping it once the table is full (`MAX_PEERS`) rather than growing --
/// this is a teaching aid for a handful of QEMU/LAN peers, not an address
/// book meant to scale. Returns whether `mac` was newly added.
fn remember_peer(mac: [u8; 6]) -> bool {
    let peers = &raw mut PEERS;
    let count = &raw mut PEER_COUNT;
    unsafe {
        if (&*peers)[..*count].contains(&mac) {
            return false;
        }
        if *count >= MAX_PEERS {
            return false;
        }
        (*peers)[*count] = mac;
        *count += 1;
        true
    }
}

/// Looks up a peer by its 1-based position in `nic peers`' printed list
/// (matching how `chat` accepts a peer number as shorthand for a full MAC
/// address).
fn peer_by_index(index: usize) -> Option<[u8; 6]> {
    let peers = &raw const PEERS;
    let count = unsafe { PEER_COUNT };
    if index == 0 || index > count {
        return None;
    }
    Some(unsafe { (*peers)[index - 1] })
}

fn print_peers() {
    let peers = &raw const PEERS;
    let count = unsafe { PEER_COUNT };
    if count == 0 {
        println!("No peers known yet. Run 'nic discover' to broadcast a probe -- other fos");
        println!("machines only answer it while their own 'chat' or 'nic discover' is running.");
        return;
    }
    println!("Known peers (use the number with 'chat' instead of a full MAC):");
    for index in 0..count {
        println!("  {}: {}", index + 1, Mac(&unsafe { (*peers)[index] }));
    }
}

/// The gap, in 100 Hz timer ticks, between re-broadcasts of the discovery
/// probe (see `nic_discover`) -- frequent enough that a peer which starts
/// `chat`/`nic discover` a few seconds late is still found before the
/// operator gives up and presses F12 (or F4/F6/F7/F8/F10), but not so frequent that the wire (or
/// the printed peer list, which only grows on genuinely new replies) is
/// spammed.
const DISCOVER_REBROADCAST_TICKS: u64 = 300;

/// Broadcasts a discovery probe (`CHAT_TAG_DISCOVER_REQUEST`) through every
/// initialized interface, re-sending it every `DISCOVER_REBROADCAST_TICKS` so
/// machines that only start `chat` or `nic discover` partway through are still
/// found, and keeps listening on every interface -- printing each newly-seen
/// peer as it replies -- until F12 (or F4/F6/F7/F8/F10) stops it, the same
/// convention `nic sniff`/`term`/`chat` all use. Replies to incoming probes
/// go back through the interface where the probe arrived. There is no
/// interrupt-driven network service in this kernel (see "Current boundaries"
/// in `AGENTS.md`), so a reply only ever comes from a fos machine whose own
/// `chat` or `nic discover` happens to be running and polling its card while
/// this loop is open -- it only finds who is listening *right now*, not who
/// exists on the network.
fn nic_discover() {
    let count = nic::count();
    if count == 0 {
        nic_status();
        return;
    }
    println!(
        "Broadcasting a discovery probe on {} interface(s) every few seconds;",
        count
    );
    println!("press F12 (or F4/F6/F7/F8/F10) to stop and see the list.");
    for index in 0..count {
        let Some(nic) = nic::get_at(index) else {
            continue;
        };
        let source = nic.mac_address();
        send_chat_control(nic, &BROADCAST_MAC, &source, CHAT_TAG_DISCOVER_REQUEST);
    }
    let mut next_broadcast = interrupts::ticks() + DISCOVER_REBROADCAST_TICKS;

    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut discovered = 0u32;
    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            break;
        }
        if interrupts::ticks() >= next_broadcast {
            for index in 0..count {
                let Some(nic) = nic::get_at(index) else {
                    continue;
                };
                let source = nic.mac_address();
                send_chat_control(nic, &BROADCAST_MAC, &source, CHAT_TAG_DISCOVER_REQUEST);
            }
            next_broadcast = interrupts::ticks() + DISCOVER_REBROADCAST_TICKS;
        }
        for index in 0..count {
            let Some(nic) = nic::get_at(index) else {
                continue;
            };
            let source = nic.mac_address();
            let Some(length) = nic.receive(&mut buffer) else {
                continue;
            };
            let frame = &buffer[..length];
            if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
                continue;
            }
            let peer_mac: [u8; 6] = frame[6..12].try_into().unwrap();
            match frame[14] {
                CHAT_TAG_DISCOVER_REPLY => {
                    if remember_peer(peer_mac) {
                        discovered += 1;
                        println!("Found peer #{discovered}: {}", Mac(&peer_mac));
                    }
                }
                // A peer's own probe reaching us proves it's listening too,
                // so it belongs in our list exactly as if it had replied to
                // ours. Reply on the same interface so discovery works across
                // isolated networks attached to different local NICs.
                CHAT_TAG_DISCOVER_REQUEST => {
                    if remember_peer(peer_mac) {
                        discovered += 1;
                        println!("Found peer #{discovered}: {}", Mac(&peer_mac));
                    }
                    send_chat_control(nic, &peer_mac, &source, CHAT_TAG_DISCOVER_REPLY);
                }
                _ => continue,
            }
        }
    }

    println!("\nStopped discovery; found {discovered} new peer(s) this run.");
    print_peers();
}

/// `chat` (no arguments) is an open, broadcast text chat: every frame it
/// sends goes to the broadcast address, and it displays any `CHAT_ETHERTYPE`
/// text frame it receives from anyone. `chat AA:BB:CC:DD:EE:FF` (or, once
/// `nic discover` has found peers, the shorter `chat N`, a 1-based number
/// from `nic peers`) instead targets one peer directly (a real unicast
/// Ethernet address, not a broadcast) and only displays lines whose
/// *source* matches that address, for a private two-machine conversation --
/// every mode reuses the exact same `Rtl8139::send`/`receive` calls, since
/// addressing here is just a different destination MAC, not a different
/// protocol.
///
/// Typed lines are sent on Enter, one line per frame, with the same
/// backspace/echo/length-limit handling the main shell prompt itself uses;
/// incoming lines are printed as they arrive, tagged with the sender's MAC.
/// Because incoming lines can arrive while a local line is only half typed,
/// they may interleave with it on screen -- a known, accepted simplification
/// for a single-scrolling-console chat, not a bug. While running, `chat`
/// also silently answers any discovery probe it sees (see `nic discover`)
/// with a one-line note, so simply leaving a chat session open is enough to
/// make a machine discoverable. Leaves through F12 (or F4/F6/F7/F8/F10) (only available from the
/// local PS/2 keyboard, same as `term`/`nic sniff`) or by typing the line
/// `/quit` (which also works from a serial-only session, since -- unlike
/// `term` -- `chat` never takes over the serial console).
#[inline(never)]
fn chat_command(arguments: &[u8]) {
    let (peer_word, remainder) = take_word(arguments);
    if !remainder.is_empty() {
        println!("Usage: chat [PEER_MAC|PEER_NUMBER]");
        return;
    }
    let peer = if peer_word.is_empty() {
        None
    } else if let Some(mac) = parse_mac(peer_word) {
        Some(mac)
    } else if let Some(index) = parse_number(peer_word) {
        match peer_by_index(index as usize) {
            Some(mac) => Some(mac),
            None => {
                println!(
                    "No peer #{index} known; run 'nic discover' first, or give a full MAC address."
                );
                return;
            }
        }
    } else {
        println!(
            "'{}' is neither a MAC address (aa:bb:cc:dd:ee:ff) nor a known peer",
            core::str::from_utf8(peer_word).unwrap_or("?")
        );
        println!("number; see 'nic peers'.");
        return;
    };

    let Some(nic) = nic_or_report() else { return };
    let destination = peer.unwrap_or(BROADCAST_MAC);
    let source = nic.mac_address();

    match peer {
        Some(mac) => {
            println!("Chatting with {} only. F12 (or F4/F6/F7/F8/F10) or", Mac(&mac));
            println!("'/quit' to leave.");
        }
        None => {
            println!("Broadcasting to everyone listening. F12 (or F4/F6/F7/F8/F10)");
            println!("or '/quit' to leave.");
        }
    }

    let mut line = [0u8; CHAT_TEXT_CAPACITY];
    let mut length = 0usize;
    let mut saw_carriage_return = false;
    let mut recv_buffer = [0u8; nic::MAX_FRAME_LEN];

    'chat: loop {
        // Serial bytes take the same path a plain keyboard character
        // would (there is no serial equivalent of the F12 (or F4/F6/F7/F8/F10) escape key);
        // only fall back to polling the keyboard -- which can also report
        // Escape -- once no serial byte is waiting.
        let event = match console::read_serial_byte() {
            Some(byte) => Some(keyboard::KeyEvent::Char(byte)),
            None => keyboard::poll_term_event(),
        };
        match event {
            Some(keyboard::KeyEvent::Escape) => break 'chat,
            Some(keyboard::KeyEvent::Char(b'\n')) if saw_carriage_return => {
                saw_carriage_return = false
            }
            Some(keyboard::KeyEvent::Char(byte @ (b'\r' | b'\n'))) => {
                saw_carriage_return = byte == b'\r';
                println!();
                if &line[..length] == b"/quit" {
                    break 'chat;
                }
                if length > 0 {
                    send_chat_line(nic, &destination, &source, &line[..length]);
                }
                length = 0;
            }
            Some(keyboard::KeyEvent::Char(8 | 127)) if length > 0 => {
                saw_carriage_return = false;
                length -= 1;
                console::erase_last_character();
            }
            Some(keyboard::KeyEvent::Char(byte @ 0x20..=0x7e)) if length < CHAT_TEXT_CAPACITY => {
                saw_carriage_return = false;
                line[length] = byte;
                length += 1;
                print!("{}", byte as char);
            }
            Some(keyboard::KeyEvent::Char(0x20..=0x7e)) => {
                saw_carriage_return = false;
                println!("\nChat lines are limited to {CHAT_TEXT_CAPACITY} characters.");
            }
            _ => {}
        }

        if let Some(received) = nic.receive(&mut recv_buffer) {
            handle_incoming_chat_frame(nic, &recv_buffer[..received], &source, peer.as_ref());
        }
    }

    println!("Left chat; back at the fos shell.");
}

fn send_chat_line(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], text: &[u8]) {
    let mut frame = [0u8; 15 + CHAT_TEXT_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = CHAT_TAG_TEXT;
    frame[15..15 + text.len()].copy_from_slice(text);
    if !nic.send(&frame[..15 + text.len()]) {
        println!("(message not sent: card reported an error, or timed out)");
    }
}

/// Sends a zero-length `CHAT_ETHERTYPE` control frame (a discovery request
/// or reply): just the Ethernet header plus the one-byte tag, since neither
/// message needs a body -- the requester learns a peer's address from the
/// reply's own source MAC, not from anything inside it.
fn send_chat_control(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], tag: u8) {
    let mut frame = [0u8; 15];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14] = tag;
    nic.send(&frame);
}

/// Handles one received frame during `chat`: a `CHAT_TAG_TEXT` line is
/// printed (filtered to `peer`'s source address in a unicast conversation;
/// shown from anyone while broadcasting) after trimming the trailing zero
/// padding `send_chat_line`/`Rtl8139::send`/`tools/send_frame.py` all add to
/// reach Ethernet's 60-byte minimum frame length. A `CHAT_TAG_DISCOVER_REQUEST`
/// is answered automatically with a reply addressed straight back to its
/// sender (never broadcast, to keep replies from also broadcasting to every
/// other listener) and remembered as a peer, exactly as if `nic discover`
/// itself had seen it; anything else (including a stray discovery reply
/// while just chatting) is ignored.
fn handle_incoming_chat_frame(
    nic: &nic::Nic,
    frame: &[u8],
    own_mac: &[u8; 6],
    peer: Option<&[u8; 6]>,
) {
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        return;
    }
    let source: [u8; 6] = frame[6..12].try_into().unwrap();
    match frame[14] {
        CHAT_TAG_TEXT => {
            if let Some(peer) = peer {
                if &source != peer {
                    return;
                }
            }
            let payload = &frame[15..];
            let text_end = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            let text =
                core::str::from_utf8(&payload[..text_end]).unwrap_or("<non-UTF-8 chat data>");
            println!("\n{}: {text}", Mac(&source));
        }
        CHAT_TAG_DISCOVER_REQUEST => {
            remember_peer(source);
            send_chat_control(nic, &source, own_mac, CHAT_TAG_DISCOVER_REPLY);
            println!("\n(replied to a discovery probe from {})", Mac(&source));
        }
        _ => {}
    }
}

/// The one currently loaded "paper tape": always `pmem::TAPE_BASE` (see its
/// doc comment) plus the tape's exact byte length. `None` until `tape load
/// disk`/`tape load net` succeeds; loading a new tape simply overwrites the
/// fixed region in place, so there's no per-tape frame bookkeeping to free.
/// The current supervisor identity map keeps this physical address directly
/// usable as a pointer -- the same convention `rtl8139.rs`'s own DMA buffers
/// rely on.
static mut TAPE: Option<(u32, u32)> = None;
static mut TAPE_ENTRY: u32 = 0;
static mut TAPE_ELF: bool = false;
static mut TAPE_HANDLE: u32 = 0;
static mut NEXT_TAPE_HANDLE: u32 = 1;
static mut TAPE_BOOT_INFO: *const BootInfo = core::ptr::null();

const TAPE_FS_OK: u32 = 0;
const TAPE_FS_NOT_FOUND: u32 = 1;
const TAPE_FS_INVALID_ARGUMENT: u32 = 2;
const TAPE_FS_BUFFER_TOO_SMALL: u32 = 3;
const TAPE_FS_DISK_ERROR: u32 = 4;
const TAPE_FS_DIRECTORY_FULL: u32 = 5;
const TAPE_FS_DATA_FULL: u32 = 6;

/// Reads/writes `TAPE` through raw pointers (`&raw const`/`&raw mut`)
/// rather than ever forming a `&`/`&mut` reference to the static, the same
/// convention `PEERS`/`PEER_COUNT` use just above -- this is a
/// single-threaded, non-reentrant shell, so the only real hazard is taking
/// a live reference across something that could alias it.
fn tape_get() -> Option<(u32, u32)> {
    unsafe { *(&raw const TAPE) }
}

fn tape_entry() -> u32 {
    unsafe { *(&raw const TAPE_ENTRY) }
}

fn install_tape(address: u32, length: u32) -> bool {
    let (entry, elf) = match load_elf32_image(address, length) {
        Ok(Some(entry)) => (entry, true),
        Ok(None) => (address, false),
        Err(message) => {
            println!("Tape rejected: {message}");
            return false;
        }
    };
    let handle = unsafe { *(&raw const NEXT_TAPE_HANDLE) };
    unsafe {
        *(&raw mut TAPE) = Some((address, length));
        *(&raw mut TAPE_ENTRY) = entry;
        *(&raw mut TAPE_ELF) = elf;
        *(&raw mut TAPE_HANDLE) = handle;
        *(&raw mut NEXT_TAPE_HANDLE) = handle.wrapping_add(1).max(1);
    }
    true
}

/// `tape [status|run [ARGS...]|load ...|save ...|send ...]`. `status`/no
/// arguments report what (if anything) is loaded; `run` executes it,
/// optionally passing everything after `run` to the tape verbatim as its
/// own `args_ptr`/`args_len` (see `tape_run`'s doc comment) -- a tape that
/// ignores those two arguments (every existing tape in this repository
/// does) is unaffected either way; `load`/`save`/`send` are documented on
/// their own handlers below.
#[inline(never)]
fn tape_command(info: &BootInfo, arguments: &[u8]) {
    let (subcommand, remainder) = take_word(arguments);
    match subcommand {
        b"" | b"status" if remainder.is_empty() => tape_status(),
        b"run" => tape_run(info, remainder),
        b"load" => tape_load_command(info, remainder),
        b"save" => tape_save_command(info, remainder),
        b"send" => tape_send_command(info, remainder),
        _ => {
            println!("Usage: tape [status|run [ARGS...]|load disk NAME|load net|");
            println!("save [diskN] NAME|send NAME PEER_MAC|PEER_NUMBER]");
        }
    }
}

fn tape_status() {
    match tape_get() {
        Some((_address, length)) => {
            let format = if unsafe { *(&raw const TAPE_ELF) } {
                "ELF32"
            } else {
                "legacy flat"
            };
            println!(
                "Tape loaded: #{}; {length} byte(s), {format},",
                unsafe { *(&raw const TAPE_HANDLE) },
            );
            println!("entry 0x{:08x}. 'tape run' to execute it.", tape_entry())
        }
        None => println!("No tape loaded. 'tape load disk NAME' or 'tape load net' to load one."),
    }
}

/// Validates `length` fits the fixed tape region and reports the fixed
/// address, or explains why it doesn't. Every tape -- whatever loads it --
/// always lands at the same `pmem::TAPE_BASE` address (see its doc
/// comment): unlike a general allocation, this is deliberate, so a tape's
/// own compiled code (assembly, C, or Rust) can address itself directly
/// instead of needing to be position-independent and locate its own
/// runtime base first. Shared by both `tape load disk` and `tape load
/// net`, since either source ends up needing the exact same buffer.
fn allocate_tape_buffer(length: u32) -> Option<u32> {
    if length > pmem::TAPE_LEN {
        println!(
            "Tape too large: {length} byte(s) exceeds the {}-byte fixed tape",
            pmem::TAPE_LEN,
        );
        println!("region at 0x{:05x}.", pmem::TAPE_BASE);
        return None;
    }
    Some(pmem::TAPE_BASE)
}

#[derive(Clone, Copy)]
struct Elf32Load {
    offset: u32,
    virtual_address: u32,
    file_size: u32,
    memory_size: u32,
}

/// Validates and materializes one ELF32 tape in its linked load region.
///
/// The first migration step intentionally accepts `ET_EXEC` images linked at
/// the historical `pmem::TAPE_BASE` and retains a raw-flat fallback. The
/// program-header-only loader keeps section metadata out of the runtime ABI;
/// `ET_DYN`/relocation support can be added later without changing callers.
fn load_elf32_image(address: u32, length: u32) -> Result<Option<u32>, &'static str> {
    if length < 52 {
        return Ok(None);
    }
    let image = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    if &image[..4] != b"\x7fELF" {
        return Ok(None);
    }
    if image[4] != 1 || image[5] != 1 || image[6] != 1 {
        return Err("expected little-endian ELF32");
    }
    let e_type = elf_u16(image, 16)?;
    if e_type != 2 {
        return Err("only fixed-address ELF32 ET_EXEC tapes are supported");
    }
    if elf_u16(image, 18)? != 3 {
        return Err("ELF image is not for i386");
    }
    let entry = elf_u32(image, 24)?;
    let program_offset = elf_u32(image, 28)?;
    let entry_size = elf_u16(image, 42)? as u32;
    let program_count = elf_u16(image, 44)? as u32;
    if entry_size < 32 || program_count == 0 || program_count > 32 {
        return Err("invalid ELF32 program-header table");
    }
    let table_end = program_offset
        .checked_add(entry_size.saturating_mul(program_count))
        .ok_or("ELF32 program-header table overflows")?;
    if table_end > length {
        return Err("ELF32 program-header table is outside the file");
    }

    let mut loads = [Elf32Load {
        offset: 0,
        virtual_address: 0,
        file_size: 0,
        memory_size: 0,
    }; 32];
    let mut load_count = 0usize;
    let tape_end = pmem::TAPE_BASE
        .checked_add(pmem::TAPE_LEN)
        .ok_or("tape address range overflows")?;
    for index in 0..program_count as usize {
        let header = program_offset as usize + index * entry_size as usize;
        let kind = elf_u32(image, header)?;
        if kind != 1 {
            continue;
        }
        let file_offset = elf_u32(image, header + 4)?;
        let virtual_address = elf_u32(image, header + 8)?;
        let file_size = elf_u32(image, header + 16)?;
        let memory_size = elf_u32(image, header + 20)?;
        if file_offset
            .checked_add(file_size)
            .ok_or("ELF32 segment overflows")?
            > length
            || file_size > memory_size
            || virtual_address < pmem::TAPE_BASE
            || virtual_address
                .checked_add(memory_size)
                .ok_or("ELF32 segment address overflows")?
                > tape_end
        {
            return Err("ELF32 load segment is outside the tape region");
        }
        if load_count == loads.len() {
            return Err("too many ELF32 load segments");
        }
        loads[load_count] = Elf32Load {
            offset: file_offset,
            virtual_address,
            file_size,
            memory_size,
        };
        load_count += 1;
    }
    if load_count == 0
        || entry < pmem::TAPE_BASE
        || entry >= tape_end
        || !loads[..load_count].iter().any(|segment| {
            entry >= segment.virtual_address
                && entry < segment.virtual_address + segment.memory_size
        })
    {
        return Err("ELF32 entry point is not inside a loadable segment");
    }

    for segment in &loads[..load_count] {
        unsafe {
            core::ptr::copy(
                (address + segment.offset) as *const u8,
                segment.virtual_address as *mut u8,
                segment.file_size as usize,
            );
            core::ptr::write_bytes(
                (segment.virtual_address + segment.file_size) as *mut u8,
                0,
                (segment.memory_size - segment.file_size) as usize,
            );
        }
    }
    Ok(Some(entry))
}

fn elf_u16(image: &[u8], offset: usize) -> Result<u16, &'static str> {
    let bytes = image
        .get(offset..offset + 2)
        .ok_or("truncated ELF32 header")?;
    Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn elf_u32(image: &[u8], offset: usize) -> Result<u32, &'static str> {
    let bytes = image
        .get(offset..offset + 4)
        .ok_or("truncated ELF32 header")?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Executes a loaded tape's validated ELF entry point (or the image base for
/// the legacy raw-flat fallback) in ring 0 on the kernel's own stack. The
/// current paging setup is supervisor-only identity mapping, not isolation,
/// so a tape remains trusted exactly as much as any other kernel code. If it
/// ends with a near `ret`, control returns cleanly to this shell; if it hangs,
/// faults, or corrupts kernel state, there is no recovery -- a reset or
/// power-cycle is required, exactly as with any other unrecoverable fault.
///
/// Called as `extern "C" fn(putc: extern "C" fn(u32), should_abort: extern
/// "C" fn() -> u32, alloc: extern "C" fn(u32) -> u32, free: extern "C"
/// fn(u32, u32), read_key: extern "C" fn() -> u32, args_ptr: u32, args_len:
/// u32, nic_mac: extern "C" fn(u32) -> u32, nic_send: extern "C" fn(u32,
/// u32) -> u32, nic_receive: extern "C" fn(u32, u32) -> u32,
/// nic_max_frame_len: extern "C" fn() -> u32, ticks: extern "C" fn() -> u64,
/// nic_count: extern "C" fn() -> u32, nic_mac_at: extern "C" fn(u32, u32)
/// -> u32, nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
/// nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
/// nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
/// terminal_position: extern "C" fn() -> u32, fs_file_length: extern "C"
/// fn(u32, u32, u32) -> u32, fs_read: extern "C" fn(u32, u32, u32, u32, u32)
/// -> u32, fs_write: extern "C" fn(u32, u32, u32, u32) -> u32, fs_remove:
/// extern "C" fn(u32, u32) -> u32)` (cdecl, twenty-two arguments), in that
/// fixed order. A tape is free
/// to declare only a leading prefix of these parameters (as
/// `primes.S`/`hello-rs` declare only `putc`, and `vga-colors` declares
/// only `putc`/`should_abort`) and ignore the rest; under cdecl this
/// remains binary-compatible, since the caller (this function) both
/// pushes and cleans up every argument regardless of how many the callee
/// actually reads. A tape needing a *later* argument (like `alloc`/`free`
/// below) must still declare every parameter before it, purely to keep
/// each argument's stack position lined up correctly -- see
/// `docs/rust-tapes-for-dummies.md` for a worked example.
/// - `console::print_byte` is passed as the first argument, giving a tape
///   a narrow, explicit way to call back into the kernel's own console
///   code to print, instead of touching VGA/COM1 hardware directly (see
///   `tapes/primes.S`'s header comment for why this one callback was
///   worth the exception). This is what makes a tape's own output show
///   up over `term net` like any other command's, and is why `tape_run`
///   no longer needs to re-sync the console's cursor state afterward: a
///   tape that only ever prints through this callback never touches the
///   hardware cursor itself, so the kernel's own cached position is
///   never stale to begin with.
/// - `combined_tape_should_abort` is passed as the second argument, giving
///   a long-running or looping tape (like `tapes/vga-colors`) a way
///   to notice a "give up" request without touching the keyboard
///   controller or network card itself. It checks two independent
///   sources, either one enough to stop the tape: a local "give up"
///   keypress (F12/F4/F6/F7/F8/F10, via `keyboard::tape_should_abort` --
///   see that function's own doc comment for why this is safe even when
///   a tape was started remotely over an active `term_net` session), and,
///   if a `term_net` session is what started this tape, a remote one too
///   (via `poll_remote_tape_abort`, since that session's own loop -- and
///   so its own `nic.receive` -- is paused for as long as this tape runs
///   and cannot notice incoming frames on its own).
/// - `tape_alloc`/`tape_free` are passed as the third and fourth
///   arguments, letting a tape reach far beyond its own fixed 4 MiB
///   static region (`pmem::TAPE_BASE`/`TAPE_LEN`) for a large working set
///   (a big sieve, a large buffer, ...) -- up to almost all of this
///   machine's RAM (see `pmem::TRACKED_BYTES`), by borrowing physically
///   contiguous frames straight from the same general pool `palloc`/
///   `pfree` operate on. See `tape_alloc`'s own doc comment for the full
///   contract (no auto-zeroing, no auto-free on return).
/// - `combined_tape_read_key` is passed as the fifth argument, giving a
///   tape a way to read actual input -- not just detect "give up" --
///   from whichever source started it, local keyboard or an active
///   `term_net` session, without knowing or caring which. See its own
///   doc comment for the full return-value encoding and, importantly,
///   why a tape should use *either* this *or* `should_abort`, never both
///   in the same loop.
/// - `args_ptr`/`args_len` are passed as the sixth and seventh arguments:
///   the address and length of whatever trailing text followed `tape run`
///   on the command line (e.g. `tape run 2 + 2` gives a tape `"2 + 2"`,
///   0 if none was given), letting a tape take simple, one-shot startup
///   parameters the way a Unix program reads `argv` -- as opposed to
///   `read_key`'s repeated, interactive input during a tape's own run.
///   This slice points directly into the command line this shell already
///   parsed (see `execute`/`LineEditor::feed`), which remains valid for
///   as long as this synchronous call lasts, so no copy is made. A tape
///   ignoring these two (every existing tape in this repository does)
///   simply never reads them, exactly like any other trailing argument
///   under cdecl.
/// - `tape_nic_mac`/`tape_nic_send`/`tape_nic_receive` are the first three
///   callbacks. They expose the active kernel-owned NIC as raw Ethernet
///   frames, so a tape can experiment with network protocols without
///   duplicating PCI discovery, DMA allocation, ring management, or
///   overflow recovery. `nic_mac` copies this machine's six-byte MAC to
///   its caller-provided buffer; `nic_send` transmits a caller-provided
///   complete frame; `nic_receive` copies one complete received frame to
///   its caller-provided buffer and returns its length, or zero when no
///   frame is waiting. Each returns zero on failure. A tape should call
///   `nic_mac` once at startup and report its failure explicitly before
///   entering a receive loop; the callbacks then reuse the same active
///   interface as the shell's `nic` and `chat` commands.
/// - `tape_nic_max_frame_len` and `tape_ticks` are the next two callbacks.
///   The first reports the largest frame `nic_receive` can return. The
///   second returns the monotonic PIT tick count used for protocol deadlines.
///   They are deliberately separate from packet operations so a shared
///   library can remain deterministic and testable with fake callbacks.
/// - `tape_nic_count`, `tape_nic_mac_at`, `tape_nic_send_at`, and
///   `tape_nic_receive_at` are appended after `ticks`. They let a newer tape
///   enumerate interfaces and keep independent raw-frame state per index.
/// - `tape_nic_set_promiscuous_at` is the final callback. It enables or
///   disables receive-all mode on one selected interface, returning zero if
///   that index is invalid. A layer-2 bridge needs this because frames
///   addressed to another host are not normally delivered to a NIC's
///   receive ring.
///
/// (A tape is still ring-0 code and technically free to ignore any of
/// these callbacks and touch hardware -- or `pmem` -- directly anyway --
/// nothing enforces this convention -- so this remains just that: a
/// convention, not a sandboxed boundary.)
extern "C" fn combined_tape_should_abort() -> u32 {
    (keyboard::tape_should_abort() != 0 || poll_remote_tape_abort()) as u32
}

/// A small FIFO of pending remote tape-input bytes, exactly analogous to
/// `console`'s own `RELAY_QUEUE` (output) or `interrupts`'s scancode queue
/// (input): a single `TERM_TAG_DATA` frame can carry many bytes at once
/// (a whole pasted or fast-typed line), but `combined_tape_read_key`
/// hands a tape back only one decoded event per call, so any bytes beyond
/// the first one a frame yields are queued here instead of discarded --
/// unlike the older `poll_remote_tape_abort`, which only ever needed to
/// detect one specific byte and could safely ignore everything else in a
/// frame's payload.
const TAPE_INPUT_QUEUE_LEN: usize = 256;
static mut TAPE_INPUT_QUEUE: [u8; TAPE_INPUT_QUEUE_LEN] = [0; TAPE_INPUT_QUEUE_LEN];
static mut TAPE_INPUT_HEAD: usize = 0;
static mut TAPE_INPUT_TAIL: usize = 0;

/// A `read_key` call and a tape's raw `nic_receive` call both ultimately
/// poll the same RTL8139 ring. When `read_key` happens to collect a chat
/// frame before the tape asks for it, retain it here instead of silently
/// discarding it. A four-frame FIFO handles short bursts and lets a protocol
/// stack drain packets in order without requiring the kernel to parse IP or
/// TCP itself.
const TAPE_NETWORK_QUEUE_LEN: usize = 4;
static mut TAPE_NETWORK_FRAMES: [[u8; nic::MAX_FRAME_LEN]; TAPE_NETWORK_QUEUE_LEN] =
    [[0; nic::MAX_FRAME_LEN]; TAPE_NETWORK_QUEUE_LEN];
static mut TAPE_NETWORK_FRAME_LENS: [usize; TAPE_NETWORK_QUEUE_LEN] = [0; TAPE_NETWORK_QUEUE_LEN];
static mut TAPE_NETWORK_HEAD: usize = 0;
static mut TAPE_NETWORK_TAIL: usize = 0;
static mut TAPE_NETWORK_COUNT: usize = 0;
/// A terminal abort can be discovered by `nic_receive` before `read_key`
/// runs. Preserve it until `read_key` returns the standard abort event.
static mut TAPE_REMOTE_ABORT_PENDING: bool = false;
/// Set by a tape that has actually asked for keyboard input. Until then,
/// ordinary terminal data seen by its raw NIC callback belongs to the
/// suspended shell editor, not to a callback the tape never consumes.
static mut TAPE_READ_KEY_USED: bool = false;

/// Queues `bytes` for later `tape_input_pop` calls, silently dropping
/// whatever doesn't fit -- the same "never corrupt accounting, just stop
/// accepting more" choice `pmem::free_big` and this file's own
/// `remember_peer` already make elsewhere, on the theory that a tape
/// falling behind this far during interactive use has bigger problems
/// than one dropped keystroke.
fn tape_input_push(bytes: &[u8]) {
    unsafe {
        for &byte in bytes {
            let next = (TAPE_INPUT_TAIL + 1) % TAPE_INPUT_QUEUE_LEN;
            if next == TAPE_INPUT_HEAD {
                break;
            }
            TAPE_INPUT_QUEUE[TAPE_INPUT_TAIL] = byte;
            TAPE_INPUT_TAIL = next;
        }
    }
}

/// Prepends the first byte of a terminal frame after `classify_remote_tape_frame`
/// has already queued its remainder. This preserves the wire's original
/// order when `nic_receive`, rather than `read_key`, consumed that frame.
fn tape_input_push_front(byte: u8) {
    unsafe {
        let previous = (TAPE_INPUT_HEAD + TAPE_INPUT_QUEUE_LEN - 1) % TAPE_INPUT_QUEUE_LEN;
        if previous != TAPE_INPUT_TAIL {
            TAPE_INPUT_HEAD = previous;
            TAPE_INPUT_QUEUE[TAPE_INPUT_HEAD] = byte;
        }
    }
}

fn tape_input_pop() -> Option<u8> {
    unsafe {
        if TAPE_INPUT_HEAD == TAPE_INPUT_TAIL {
            return None;
        }
        let byte = TAPE_INPUT_QUEUE[TAPE_INPUT_HEAD];
        TAPE_INPUT_HEAD = (TAPE_INPUT_HEAD + 1) % TAPE_INPUT_QUEUE_LEN;
        Some(byte)
    }
}

fn tape_network_frame_push(frame: &[u8]) {
    unsafe {
        if frame.len() <= nic::MAX_FRAME_LEN && TAPE_NETWORK_COUNT < TAPE_NETWORK_QUEUE_LEN {
            core::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                (&raw mut TAPE_NETWORK_FRAMES[TAPE_NETWORK_TAIL]).cast::<u8>(),
                frame.len(),
            );
            TAPE_NETWORK_FRAME_LENS[TAPE_NETWORK_TAIL] = frame.len();
            TAPE_NETWORK_TAIL = (TAPE_NETWORK_TAIL + 1) % TAPE_NETWORK_QUEUE_LEN;
            TAPE_NETWORK_COUNT += 1;
        }
    }
}

fn tape_network_frame_pop(out: &mut [u8]) -> Option<usize> {
    unsafe {
        if TAPE_NETWORK_COUNT == 0 {
            return None;
        }
        let length = TAPE_NETWORK_FRAME_LENS[TAPE_NETWORK_HEAD];
        if out.len() < length {
            return None;
        }
        core::ptr::copy_nonoverlapping(
            (&raw const TAPE_NETWORK_FRAMES[TAPE_NETWORK_HEAD]).cast::<u8>(),
            out.as_mut_ptr(),
            length,
        );
        TAPE_NETWORK_FRAME_LENS[TAPE_NETWORK_HEAD] = 0;
        TAPE_NETWORK_HEAD = (TAPE_NETWORK_HEAD + 1) % TAPE_NETWORK_QUEUE_LEN;
        TAPE_NETWORK_COUNT -= 1;
        Some(length)
    }
}

enum RemoteTapeFrame {
    Input(u32),
    Consumed,
    PassThrough,
}

/// Separates a `term net` control/input frame from an ordinary raw Ethernet
/// frame while a tape is running. `read_key` turns `Input` into an event;
/// `nic_receive` queues it for that later `read_key` call. Everything else
/// remains available to a networking tape through `PassThrough`.
fn classify_remote_tape_frame(peer: &[u8; 6], frame: &[u8]) -> RemoteTapeFrame {
    if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
        return RemoteTapeFrame::PassThrough;
    }
    let frame_source: [u8; 6] = frame[6..12].try_into().unwrap();
    // A networking tape owns chat/discovery framing itself. Leave discovery
    // probes in its raw receive stream so it can update its own peer table
    // and send exactly one protocol reply, rather than having this remote-
    // input bridge consume the frame before the tape ever sees it.
    if frame[14] == CHAT_TAG_DISCOVER_REQUEST {
        return RemoteTapeFrame::PassThrough;
    }
    if &frame_source != peer {
        return RemoteTapeFrame::PassThrough;
    }
    match frame[14] {
        TERM_TAG_BYE => {
            unsafe { PENDING_REMOTE_BYE = true };
            RemoteTapeFrame::Input(TAPE_EVENT_ABORT)
        }
        TERM_TAG_DATA => {
            let payload = &frame[15..];
            let end = payload
                .iter()
                .rposition(|&byte| byte != 0)
                .map_or(0, |index| index + 1);
            let data = &payload[..end];
            if data.contains(&REMOTE_ABORT_BYTE) {
                return RemoteTapeFrame::Input(TAPE_EVENT_ABORT);
            }
            let Some((&first, rest)) = data.split_first() else {
                return RemoteTapeFrame::Consumed;
            };
            tape_input_push(rest);
            RemoteTapeFrame::Input(TAPE_EVENT_CHAR_BASE | u32::from(first))
        }
        _ => RemoteTapeFrame::PassThrough,
    }
}

/// The one non-zero "nothing to report" event `combined_tape_read_key`
/// never returns -- reserved so `0` unambiguously means "no input right
/// now", matching `should_abort`'s own "0 means keep going" convention.
const TAPE_EVENT_NONE: u32 = 0;
/// Returned by `combined_tape_read_key` for a "give up" signal -- a local
/// escape keypress or a remote `term_net` peer's abort byte/disconnect --
/// exactly the same trigger `combined_tape_should_abort` already reports
/// as `true`, just encoded as a distinct value here so one event can mean
/// either "stop" or "here is a character" without a tape needing a second
/// callback to tell them apart.
const TAPE_EVENT_ABORT: u32 = 1;
/// Everything from `TAPE_EVENT_CHAR_BASE` up encodes an ordinary input
/// byte: subtract this base back off to recover it. Starts above `0`/`1`
/// so every valid encoded byte (`0..=255`) stays distinguishable from
/// both reserved values without needing a second return channel.
const TAPE_EVENT_CHAR_BASE: u32 = 0x100;

/// Polls the network card once, on behalf of `combined_tape_read_key`,
/// for a byte typed by the `term_net` peer that started this tape --
/// mirrors `poll_remote_tape_abort`'s framing/discovery-probe/output-
/// relaying handling closely (see that function's doc comment for why
/// polling here is safe even mid-tape), but where that function only
/// ever needed to notice one specific byte and could discard the rest of
/// a payload, this one keeps every byte: the first is returned
/// immediately, any more are queued in `TAPE_INPUT_QUEUE` for the next
/// call. A payload containing the abort byte (`REMOTE_ABORT_BYTE`)
/// anywhere is still treated exactly like `poll_remote_tape_abort`
/// treats it -- the whole frame is dropped as a "give up" signal, never
/// fed to a tape as ordinary characters, so a peer's escape keypress can
/// never accidentally end up echoed into a tape's own input.
///
/// Output relaying happens first, unconditionally, even when a queued
/// `TAPE_INPUT_QUEUE` byte lets this call return immediately afterward --
/// otherwise a fast-typing peer could keep this function returning from
/// the queue for several calls in a row without ever reaching the
/// network-polling code below, leaving whatever the tape had already
/// printed (e.g. a prompt) stuck in the relay queue for longer than
/// necessary.
fn poll_remote_tape_input() -> Option<u32> {
    if unsafe { TAPE_REMOTE_ABORT_PENDING } {
        unsafe { TAPE_REMOTE_ABORT_PENDING = false };
        return Some(TAPE_EVENT_ABORT);
    }
    if console::relay_enabled() {
        if let (Some(peer), Some(source)) =
            (unsafe { ACTIVE_TERM_PEER }, unsafe { ACTIVE_TERM_SOURCE })
        {
            if let Ok(nic) = nic::get() {
                relay_pending_tape_output(nic, &peer, &source);
            }
        }
    }
    if let Some(byte) = tape_input_pop() {
        return Some(TAPE_EVENT_CHAR_BASE | u32::from(byte));
    }
    if !console::relay_enabled() {
        return None;
    }
    let Some(peer) = (unsafe { ACTIVE_TERM_PEER }) else {
        return None;
    };
    let Ok(nic) = nic::get() else { return None };
    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let Some(received) = nic.receive(&mut buffer) else {
        return None;
    };
    let frame = &buffer[..received];
    match classify_remote_tape_frame(&peer, frame) {
        RemoteTapeFrame::Input(event) => Some(event),
        RemoteTapeFrame::Consumed => None,
        RemoteTapeFrame::PassThrough => {
            tape_network_frame_push(frame);
            None
        }
    }
}

/// Copies the active NIC's MAC into a six-byte tape buffer. A tape calls this
/// once before networking so a missing or failed controller has one explicit,
/// checkable error path instead of appearing as an endless empty receive loop.
extern "C" fn tape_nic_mac(out: u32) -> u32 {
    if out == 0 {
        return 0;
    }
    let Ok(nic) = nic::get() else { return 0 };
    unsafe {
        core::slice::from_raw_parts_mut(out as *mut u8, 6).copy_from_slice(&nic.mac_address());
    }
    1
}

/// Copies the selected NIC's MAC into a six-byte tape buffer.
extern "C" fn tape_nic_mac_at(index: u32, out: u32) -> u32 {
    if out == 0 {
        return 0;
    }
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    unsafe {
        core::slice::from_raw_parts_mut(out as *mut u8, 6).copy_from_slice(&nic.mac_address());
    }
    1
}

/// Sends one complete raw Ethernet frame through the active NIC.
/// Invalid lengths and controller/transmit failures are reported as zero;
/// callers own their input buffer and should print a useful error themselves.
extern "C" fn tape_nic_send(frame: u32, length: u32) -> u32 {
    if frame == 0 || !(14..=nic::MAX_FRAME_LEN as u32).contains(&length) {
        return 0;
    }
    let Ok(nic) = nic::get() else { return 0 };
    let frame = unsafe { core::slice::from_raw_parts(frame as *const u8, length as usize) };
    nic.send(frame) as u32
}

/// Sends one complete raw Ethernet frame through a selected NIC.
extern "C" fn tape_nic_send_at(index: u32, frame: u32, length: u32) -> u32 {
    if frame == 0 || !(14..=nic::MAX_FRAME_LEN as u32).contains(&length) {
        return 0;
    }
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    let frame = unsafe { core::slice::from_raw_parts(frame as *const u8, length as usize) };
    nic.send(frame) as u32
}

/// Returns the number of initialized Ethernet interfaces.
extern "C" fn tape_nic_count() -> u32 {
    nic::count() as u32
}

/// Reports the maximum Ethernet frame length accepted by the kernel-owned
/// driver. A shared tape networking library uses this instead of duplicating
/// the RTL8139 limit, so its receive buffers stay synchronized with the
/// kernel's hardware boundary.
extern "C" fn tape_nic_max_frame_len() -> u32 {
    nic::MAX_FRAME_LEN as u32
}

/// Returns the monotonic PIT tick counter used by the kernel. Protocol
/// retransmission and connection deadlines should use this callback rather
/// than programming a timer or reading PIT hardware from a tape.
extern "C" fn tape_ticks() -> u64 {
    interrupts::ticks()
}

/// Copies one raw Ethernet frame into `out`, returning its full length or
/// zero when none is waiting, the caller buffer is too small, or the NIC
/// cannot be used. A terminal frame consumed here is queued for `read_key`
/// rather than exposed as chat traffic, preserving remote input while a
/// networking tape runs inside `term net`. A too-small-buffer poll leaves
/// the frame in the deferred FIFO for a later retry.
extern "C" fn tape_nic_receive(out: u32, capacity: u32) -> u32 {
    if out == 0 || capacity as usize > nic::MAX_FRAME_LEN {
        return 0;
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out as *mut u8, capacity as usize) };
    if let Some(length) = tape_network_frame_pop(out) {
        return length as u32;
    }
    let Ok(nic) = nic::get() else { return 0 };
    let mut frame = [0u8; nic::MAX_FRAME_LEN];
    let Some(length) = nic.receive(&mut frame) else {
        return 0;
    };

    if console::relay_enabled() {
        if let Some(peer) = unsafe { ACTIVE_TERM_PEER } {
            if !unsafe { TAPE_READ_KEY_USED }
                && frame[..length].len() >= 15
                && frame[12..14] == CHAT_ETHERTYPE
                && frame[6..12] == peer
                && frame[14] == TERM_TAG_DATA
                && !frame[15..length].contains(&REMOTE_ABORT_BYTE)
            {
                defer_active_term_input(&frame[..length]);
                return 0;
            }
            match classify_remote_tape_frame(&peer, &frame[..length]) {
                RemoteTapeFrame::Input(TAPE_EVENT_ABORT) => {
                    unsafe { TAPE_REMOTE_ABORT_PENDING = true };
                    return 0;
                }
                RemoteTapeFrame::Input(event) => {
                    tape_input_push_front((event & 0xff) as u8);
                    return 0;
                }
                RemoteTapeFrame::Consumed => return 0,
                RemoteTapeFrame::PassThrough => {}
            }
        }
    }
    if out.len() < length {
        // Keep the frame available for a retry with the advertised maximum
        // size instead of consuming it because this poll used a small
        // caller-provided buffer.
        tape_network_frame_push(&frame[..length]);
        return 0;
    }
    out[..length].copy_from_slice(&frame[..length]);
    length as u32
}

/// Receives one frame from a selected NIC. The active interface uses the
/// existing terminal-aware path; other interfaces are exposed directly to
/// the tape without being mistaken for the active `term net` input channel.
extern "C" fn tape_nic_receive_at(index: u32, out: u32, capacity: u32) -> u32 {
    if out == 0 || capacity as usize > nic::MAX_FRAME_LEN {
        return 0;
    }
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    if index as usize == nic::active_index() {
        return tape_nic_receive(out, capacity);
    }
    let out = unsafe { core::slice::from_raw_parts_mut(out as *mut u8, capacity as usize) };
    let mut frame = [0u8; nic::MAX_FRAME_LEN];
    let Some(length) = nic.receive(&mut frame) else {
        return 0;
    };
    if out.len() < length {
        return 0;
    }
    out[..length].copy_from_slice(&frame[..length]);
    length as u32
}

/// Enables or disables receive-all mode for one selected NIC. This is
/// intentionally per-interface: a multi-port layer-2 tape must see frames
/// addressed to hosts behind its other ports, while ordinary protocol tapes
/// should keep the backend's normal address filtering.
extern "C" fn tape_nic_set_promiscuous_at(index: u32, enabled: u32) -> u32 {
    let Some(nic) = nic::get_at(index as usize) else {
        return 0;
    };
    nic.set_promiscuous(enabled != 0);
    1
}

/// `extern "C" fn() -> u32` callback giving a tape actual interactive
/// input, not just "give up" detection -- passed as the tape ABI's fifth
/// argument (see `tape_run`'s doc comment). Non-blocking: called once per
/// polling iteration by a tape's own loop, exactly like `should_abort`,
/// returning immediately either way.
///
/// Return value, checked in this order:
/// - `0` (`TAPE_EVENT_NONE`): nothing waiting right now; keep polling.
/// - `1` (`TAPE_EVENT_ABORT`): the same "give up" signal
///   `combined_tape_should_abort` reports as `true` -- a local escape
///   keypress, or (if a `term_net` session started this tape) the remote
///   peer's abort byte or a disconnect.
/// - `0x100` (`TAPE_EVENT_CHAR_BASE`) or above: an ordinary input byte,
///   recovered with `value - 0x100` (or `value & 0xff`, equivalently) --
///   from whichever source produced it, local keyboard or an active
///   `term_net` session's `TERM_TAG_DATA` payload, with no way for a tape
///   to tell which after the fact (nor any reason to want to: both are
///   just "the user typed this").
///
/// A decoded backspace (ASCII `8`) also triggers `console::
/// erase_last_character()` here, before returning the event -- the one
/// piece of this a tape cannot safely do itself even with the `putc`
/// callback, since plain `console::write_byte` does not special-case
/// `0x08` the way `erase_last_character` does (rewinding the VGA cursor
/// and emitting the `\x08 \x08` sequence a real serial terminal expects).
/// Doing it here, unconditionally, keeps a tape's own backspace handling
/// as simple as "decrement my buffer length" regardless of whether the
/// keystroke was typed locally or arrived over the network -- exactly
/// mirroring how `LineEditor::feed` already handles backspace for the
/// ordinary shell prompt.
///
/// Shares its two input sources with `should_abort`
/// (`keyboard::poll_term_event`'s scancode queue, and a `term_net`
/// session's own network frames) but is a genuinely separate consumer of
/// each: calling both callbacks from the same tape in the same loop will
/// race them for input and intermittently lose keystrokes to whichever
/// one happens to poll first. A tape wanting real input should call only
/// this one -- checking for `TAPE_EVENT_ABORT` covers everything
/// `should_abort` would have told it anyway -- not both.
extern "C" fn combined_tape_read_key() -> u32 {
    unsafe { TAPE_READ_KEY_USED = true };
    if let Some(event) = keyboard::poll_term_event() {
        let code = match event {
            keyboard::KeyEvent::Escape => TAPE_EVENT_ABORT,
            keyboard::KeyEvent::Char(byte) => TAPE_EVENT_CHAR_BASE | u32::from(byte),
            // Meaningless outside `term_printer`'s own typing loop; no
            // tape ABI event corresponds to it, so just poll again.
            keyboard::KeyEvent::KillLine => return combined_tape_read_key(),
        };
        if code == TAPE_EVENT_CHAR_BASE | 8 {
            console::erase_last_character();
        }
        return code;
    }
    match poll_remote_tape_input() {
        Some(code) => {
            if code == TAPE_EVENT_CHAR_BASE | 8 {
                console::erase_last_character();
            }
            code
        }
        None => TAPE_EVENT_NONE,
    }
}

/// Hands a tape a fresh, physically contiguous block of at least `bytes`
/// bytes (rounded up to whole 1 MiB chunks), from `pmem`'s separate,
/// coarse-grained "big pool" (`pmem::allocate_big_contiguous`/`free_big`)
/// -- everything from `pmem::BIG_POOL_BASE` up to `pmem::BIG_POOL_END`
/// (2 GiB by default), entirely apart from the fine 4 KiB frame pool
/// `palloc`/`pfree`/the fixed 4 MiB tape region draw from. Passed as the
/// tape ABI's third argument (see `tape_run`'s doc comment). See
/// `pmem::TRACKED_BYTES`'s doc comment for why this second, coarser pool
/// exists at all instead of simply growing the fine bitmap: doing that
/// would have grown `kernel.bin` itself (a flat binary's `.bss` is baked
/// into the file) beyond the old single-transfer kernel-load ceiling.
///
/// Returns physical address `0` on failure (not enough contiguous free
/// memory) -- never a valid chunk address, since nothing below
/// `pmem::BIG_POOL_BASE` is ever handed out by this callback. Unlike a
/// `static mut` array (whose bytes a flat binary's own loading always
/// zeroes, see `entry.S`'s `.bss` zeroing stub in every Rust tape), these
/// chunks are **not** zeroed -- they may hold whatever a previous
/// allocation left behind -- so a tape relying on its buffer starting at
/// zero must zero it itself (see `tapes/bigsieve/src/main.rs`).
///
/// A tape that calls this must later call `tape_free` with the exact same
/// `bytes` value (or leak those chunks forever) -- there is no automatic
/// cleanup on return, the same "trusted exactly as much as any other
/// kernel code" reasoning `tape_run`'s own doc comment already gives for
/// tapes in general, and the same manual-only contract `palloc`/`pfree`
/// already have from the shell.
extern "C" fn tape_alloc(bytes: u32) -> u32 {
    let chunks = bytes.div_ceil(pmem::BIG_CHUNK_SIZE) as usize;
    pmem::allocate_big_contiguous(chunks).unwrap_or(0)
}

/// `tape_alloc`'s exact opposite: releases the same `bytes` (rounded up
/// to whole chunks the same way `tape_alloc` rounded when allocating)
/// back to the big pool, starting at `address`. Passed as the tape ABI's
/// fourth argument (see `tape_run`'s doc comment). `pmem::free_big`
/// already silently ignores an out-of-range or already-free chunk rather
/// than corrupting accounting, so a slightly-wrong `bytes` value here is
/// safe, if still a bug worth fixing in the tape itself.
extern "C" fn tape_free(address: u32, bytes: u32) {
    let chunks = bytes.div_ceil(pmem::BIG_CHUNK_SIZE) as usize;
    pmem::free_big(address, chunks);
}

fn tape_boot_info() -> Option<&'static BootInfo> {
    let pointer = unsafe { TAPE_BOOT_INFO };
    (!pointer.is_null()).then(|| unsafe { &*pointer })
}

/// A FOSFS name is at most 16 bytes; a `diskN/` selector prefix (matching
/// `tape run`'s `@diskN/NAME` and the shell's `diskN` command selectors)
/// adds a handful more, so this is a generous round bound, not a precise
/// one -- the real length/charset checks happen on the two halves
/// separately once split, in [`tape_fs_reference`].
const TAPE_FS_REFERENCE_CAPACITY: usize = 32;

/// Resolves a tape-supplied file reference into a disk device and a plain
/// FOSFS name, exactly mirroring `tape run`'s `@diskN/NAME` convention: a
/// bare name (no `/`) always means the boot disk, matching every existing
/// tape's behavior before this existed; a `diskN/NAME` reference selects an
/// attached secondary disk instead. A literal `/` is unambiguous as the
/// separator since FOSFS names may only contain letters, digits, `.`, `_`,
/// and `-`, so a real file name can never collide with it.
fn tape_fs_reference(pointer: u32, length: u32) -> Option<(disk::Device, &'static [u8])> {
    let length = usize::try_from(length).ok()?;
    if pointer == 0 || length == 0 || length > TAPE_FS_REFERENCE_CAPACITY {
        return None;
    }
    let reference = unsafe { core::slice::from_raw_parts(pointer as *const u8, length) };
    let (device, name) = match reference.iter().position(|&byte| byte == b'/') {
        Some(slash) => {
            let device = strip_disk_selector(&reference[..slash])?;
            (device, &reference[slash + 1..])
        }
        None => (disk::Device::Boot, reference),
    };
    if name.is_empty()
        || name.len() > 16
        || !name
            .iter()
            .all(|byte| matches!(*byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
    {
        return None;
    }
    Some((device, name))
}

fn tape_fs_buffer(pointer: u32, length: u32) -> Option<&'static mut [u8]> {
    let length = usize::try_from(length).ok()?;
    if length == 0 {
        return Some(&mut []);
    }
    (pointer != 0).then(|| unsafe {
        core::slice::from_raw_parts_mut(pointer as *mut u8, length)
    })
}

/// Returns a FOSFS file's exact byte length through the append-only tape ABI.
/// The output length is written unaligned because tape buffers are not
/// required to have a particular alignment. `name` may be a bare FOSFS name
/// (boot disk) or a `diskN/NAME` reference (an attached secondary disk); see
/// [`tape_fs_reference`].
extern "C" fn tape_fs_file_length(name: u32, name_length: u32, out_length: u32) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    if out_length == 0 {
        return TAPE_FS_INVALID_ARGUMENT;
    }
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => {
            unsafe { core::ptr::write_unaligned(out_length as *mut u64, length) };
            TAPE_FS_OK
        }
        Ok(None) => TAPE_FS_NOT_FOUND,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

/// Reads a complete FOSFS file into a tape buffer. The output length is
/// written only on success; callers should query `tape_fs_file_length` first
/// when they need to allocate a precisely sized buffer. `name` may be a bare
/// FOSFS name (boot disk) or a `diskN/NAME` reference (an attached secondary
/// disk); see [`tape_fs_reference`].
extern "C" fn tape_fs_read(
    name: u32,
    name_length: u32,
    buffer: u32,
    capacity: u32,
    out_length: u32,
) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    if out_length == 0 {
        return TAPE_FS_INVALID_ARGUMENT;
    }
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    let length = match fosfs::file_length(info, device, name) {
        Ok(Some(length)) => length,
        Ok(None) => return TAPE_FS_NOT_FOUND,
        Err(_) => return TAPE_FS_DISK_ERROR,
    };
    if length > u64::from(capacity) {
        return TAPE_FS_BUFFER_TOO_SMALL;
    }
    let Some(buffer) = tape_fs_buffer(buffer, capacity) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    match fosfs::read(info, device, name, buffer) {
        Ok(Some(read_length)) => {
            unsafe { core::ptr::write_unaligned(out_length as *mut u32, read_length as u32) };
            TAPE_FS_OK
        }
        Ok(None) => TAPE_FS_NOT_FOUND,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

/// Replaces or creates one complete FOSFS file from a tape buffer. `name` may
/// be a bare FOSFS name (boot disk) or a `diskN/NAME` reference (an attached
/// secondary disk); see [`tape_fs_reference`].
extern "C" fn tape_fs_write(name: u32, name_length: u32, buffer: u32, length: u32) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    let Some(buffer) = tape_fs_buffer(buffer, length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    match fosfs::write(info, device, name, buffer) {
        Ok(fosfs::WriteOutcome::Written) => TAPE_FS_OK,
        Ok(fosfs::WriteOutcome::InvalidName) => TAPE_FS_INVALID_ARGUMENT,
        Ok(fosfs::WriteOutcome::DirectoryFull) => TAPE_FS_DIRECTORY_FULL,
        Ok(fosfs::WriteOutcome::DataFull) => TAPE_FS_DATA_FULL,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

/// Removes one FOSFS file, returning `TAPE_FS_NOT_FOUND` when absent. `name`
/// may be a bare FOSFS name (boot disk) or a `diskN/NAME` reference (an
/// attached secondary disk); see [`tape_fs_reference`].
extern "C" fn tape_fs_remove(name: u32, name_length: u32) -> u32 {
    let Some((device, name)) = tape_fs_reference(name, name_length) else {
        return TAPE_FS_INVALID_ARGUMENT;
    };
    let Some(info) = tape_boot_info() else {
        return TAPE_FS_DISK_ERROR;
    };
    match fosfs::remove(info, device, name) {
        Ok(true) => TAPE_FS_OK,
        Ok(false) => TAPE_FS_NOT_FOUND,
        Err(_) => TAPE_FS_DISK_ERROR,
    }
}

enum TapeArgumentSource<'a> {
    Direct(&'a [u8]),
    File(disk::Device, &'a [u8]),
    Invalid,
}

/// `@NAME` reads the config file from the boot disk, matching the historical
/// syntax; `@diskN/NAME` reads it from an attached secondary disk instead.
/// A literal `/` is unambiguous as the disk/name separator here -- FOSFS
/// names may only contain letters, digits, `.`, `_`, and `-`, so `/` can
/// never appear in a real file name to collide with this.
fn tape_argument_source(arguments: &[u8]) -> TapeArgumentSource<'_> {
    let (first, remainder) = take_word(arguments);
    let (candidate, trailing) = if first == b"router" {
        take_word(remainder)
    } else {
        (first, remainder)
    };
    if !candidate.starts_with(b"@") {
        return TapeArgumentSource::Direct(arguments);
    }
    if candidate.len() == 1 || !trailing.is_empty() {
        return TapeArgumentSource::Invalid;
    }
    let reference = &candidate[1..];
    let (device, name) = match reference.iter().position(|&byte| byte == b'/') {
        Some(slash) => {
            let Some(device) = strip_disk_selector(&reference[..slash]) else {
                return TapeArgumentSource::Invalid;
            };
            (device, &reference[slash + 1..])
        }
        None => (disk::Device::Boot, reference),
    };
    if name.is_empty() {
        return TapeArgumentSource::Invalid;
    }
    TapeArgumentSource::File(device, name)
}

#[inline(never)]
fn tape_run(info: &BootInfo, arguments: &[u8]) {
    let Some((address, length)) = tape_get() else {
        println!("No tape loaded; use 'tape load disk NAME' or 'tape load net' first.");
        return;
    };
    let mut file_buffer = [0u8; TAPE_ARGUMENT_FILE_CAPACITY];
    let arguments = match tape_argument_source(arguments) {
        TapeArgumentSource::Direct(arguments) => arguments,
        TapeArgumentSource::Invalid => {
            println!("Usage: tape run [ARGS...] or tape run router @CONFIG_FILE");
            println!("(or @diskN/CONFIG_FILE)");
            return;
        }
        TapeArgumentSource::File(device, name) => {
            let length = match fosfs::file_length(info, device, name) {
                Ok(Some(length)) => length,
                Ok(None) => {
                    println!(
                        "Router configuration file '{}' was not found on disk{}.",
                        core::str::from_utf8(name).unwrap_or("?"),
                        device_number(device)
                    );
                    return;
                }
                Err(error) => {
                    println!("FOSFS disk error while reading router configuration: {error:?}");
                    return;
                }
            };
            if length > TAPE_ARGUMENT_FILE_CAPACITY as u64 {
                println!(
                    "Router configuration file is too large: {length} bytes (maximum {TAPE_ARGUMENT_FILE_CAPACITY})."
                );
                return;
            }
            let length = length as usize;
            match fosfs::read(info, device, name, &mut file_buffer[..length]) {
                Ok(Some(_)) => &file_buffer[..length],
                Ok(None) => {
                    println!(
                        "Router configuration file '{}' was not found on disk{}.",
                        core::str::from_utf8(name).unwrap_or("?"),
                        device_number(device)
                    );
                    return;
                }
                Err(error) => {
                    println!("FOSFS disk error while reading router configuration: {error:?}");
                    return;
                }
            }
        }
    };
    println!("Running {length}-byte tape at 0x{address:05x} in ring 0, no");
    println!("memory protection or fault recovery.");
    // No tape's pending remote keystrokes, abort, or raw frame belongs to
    // the next one. Clear this cross-callback state before every entry,
    // including tapes launched locally after a prior `term net` session.
    unsafe {
        TAPE_INPUT_HEAD = 0;
        TAPE_INPUT_TAIL = 0;
        TAPE_NETWORK_HEAD = 0;
        TAPE_NETWORK_TAIL = 0;
        TAPE_NETWORK_COUNT = 0;
        TAPE_REMOTE_ABORT_PENDING = false;
        TAPE_READ_KEY_USED = false;
    }
    type TapeEntry = extern "C" fn(
        putc: extern "C" fn(u32),
        should_abort: extern "C" fn() -> u32,
        alloc: extern "C" fn(u32) -> u32,
        free: extern "C" fn(u32, u32),
        read_key: extern "C" fn() -> u32,
        args_ptr: u32,
        args_len: u32,
        nic_mac: extern "C" fn(u32) -> u32,
        nic_send: extern "C" fn(u32, u32) -> u32,
        nic_receive: extern "C" fn(u32, u32) -> u32,
        nic_max_frame_len: extern "C" fn() -> u32,
        ticks: extern "C" fn() -> u64,
        nic_count: extern "C" fn() -> u32,
        nic_mac_at: extern "C" fn(u32, u32) -> u32,
        nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
        nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
        nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
        terminal_position: extern "C" fn() -> u32,
        fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
        fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
        fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
        fs_remove: extern "C" fn(u32, u32) -> u32,
    );
    let entry: TapeEntry =
        unsafe { core::mem::transmute::<usize, TapeEntry>(tape_entry() as usize) };
    unsafe {
        TAPE_BOOT_INFO = info as *const BootInfo;
    }
    entry(
        console::print_byte,
        combined_tape_should_abort,
        tape_alloc,
        tape_free,
        combined_tape_read_key,
        arguments.as_ptr() as u32,
        arguments.len() as u32,
        tape_nic_mac,
        tape_nic_send,
        tape_nic_receive,
        tape_nic_max_frame_len,
        tape_ticks,
        tape_nic_count,
        tape_nic_mac_at,
        tape_nic_send_at,
        tape_nic_receive_at,
        tape_nic_set_promiscuous_at,
        console::terminal_position,
        tape_fs_file_length,
        tape_fs_read,
        tape_fs_write,
        tape_fs_remove,
    );
    unsafe {
        TAPE_BOOT_INFO = core::ptr::null();
    }
    console::reset_terminal_output();
    println!("Tape returned; back at the fos shell.");
}

#[inline(never)]
fn tape_load_command(info: &BootInfo, arguments: &[u8]) {
    let (kind, remainder) = take_word(arguments);
    match kind {
        b"net" if remainder.is_empty() => tape_load_net(),
        b"disk" => {
            let (name, extra) = take_word(remainder);
            if name.is_empty() || !extra.is_empty() {
                println!("Usage: tape load disk NAME");
                return;
            }
            tape_load_disk(info, disk::Device::Boot, name);
        }
        // `disk` above is really just `disk1` spelled the historical way;
        // any other `diskN` word (`disk2`, `disk3`, ...) names an attached
        // secondary disk exactly like every other disk command's selector.
        _ if strip_disk_selector(kind).is_some() => {
            let device = strip_disk_selector(kind).unwrap();
            let (name, extra) = take_word(remainder);
            if name.is_empty() || !extra.is_empty() {
                println!("Usage: tape load diskN NAME");
                return;
            }
            tape_load_disk(info, device, name);
        }
        _ => println!("Usage: tape load [net|disk NAME|diskN NAME]"),
    }
}

/// Loads a complete flat FOSFS file straight into the fixed tape buffer.
/// Multi-sector files are read one BIOS sector at a time, and the file must
/// fit the same fixed region used by `tape load net`.
#[inline(never)]
fn tape_load_disk(info: &BootInfo, device: disk::Device, name: &[u8]) {
    match fosfs::file_length(info, device, name) {
        Ok(Some(length)) if length == 0 => {
            println!(
                "'{}' is empty; nothing to load.",
                core::str::from_utf8(name).unwrap_or("?")
            );
        }
        Ok(Some(length)) if length > u32::MAX as u64 => {
            println!("Tape is too large for this 32-bit tape loader.");
        }
        Ok(Some(length)) => {
            let length = length as usize;
            let Some(address) = allocate_tape_buffer(length as u32) else {
                return;
            };
            let destination =
                unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
            match fosfs::read(info, device, name, destination) {
                Ok(Some(_)) => {
                    if !install_tape(address, length as u32) {
                        return;
                    }
                    println!(
                        "Loaded {length} byte(s) from '{}' at 0x{address:05x}.",
                        core::str::from_utf8(name).unwrap_or("?")
                    );
                    println!("'tape run' to execute it.");
                }
                Ok(None) => println!("File not found."),
                Err(error) => println!("FOSFS disk error: {error:?}"),
            }
        }
        Ok(None) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// Writes the currently loaded tape's exact bytes out to FOSFS, the
/// missing counterpart to `tape load disk`/`tape load net`: a tape
/// received over the network (`tape load net`) previously had no way to
/// persist it, so a fresh `tape load net` transfer -- with its real,
/// per-run pacing/reliability cost -- was the only way to run it again
/// after a reboot. Works identically regardless of how the tape got into
/// the fixed buffer in the first place (disk load, network load, or even
/// right after an earlier `tape save`), since it just re-reads whatever
/// `tape_get()` currently reports, exactly the same "whatever's resident"
/// contract `tape run` itself already relies on. Unlike `copy_file`, this
/// never reuses the buffer as scratch space for something else, so the
/// loaded tape's own bookkeeping (`TAPE`/`TAPE_ENTRY`/`TAPE_HANDLE`) is
/// left completely untouched -- `tape run` still works immediately
/// afterward with no need to `tape load` it back first.
#[inline(never)]
fn tape_save_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: tape save [diskN] NAME");
        return;
    }
    let Some((address, length)) = tape_get() else {
        println!("No tape loaded; use 'tape load disk NAME' or 'tape load net' first.");
        return;
    };
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    match fosfs::write(info, device, name, bytes) {
        Ok(fosfs::WriteOutcome::Written) => println!(
            "Saved {length} byte(s) of the loaded tape to disk{} '{}'.",
            device_number(device),
            core::str::from_utf8(name).unwrap_or("?")
        ),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// One tape transfer in progress during `tape load net`: which sender it's
/// locked onto (so unrelated chat/discover/tape traffic from anyone else
/// can't corrupt it mid-transfer), where its bytes are landing, how long
/// it's declared to be, and how much of it (by sequence number and byte
/// count) has arrived so far.
struct TapeTransfer {
    source: [u8; 6],
    address: u32,
    length: u32,
    expected_seq: u32,
    received: u32,
}

/// Waits for a tape to arrive over the network: a `TAPE_TAG_START` frame
/// declares the total length, then `TAPE_TAG_DATA` frames each carry an
/// explicit sequence number and up to `TAPE_CHUNK_LEN` bytes, until a
/// `TAPE_TAG_STOP` frame closes the transfer -- see the `TAPE_TAG_*`
/// constants. The offset and expected byte count for each chunk come from
/// its sequence number and the transfer's declared total length, never
/// from the received Ethernet frame's own (possibly hardware- or
/// software-padded) length, so a genuine binary tape's own trailing zero
/// bytes are never mistaken for padding -- unlike `chat`'s text frames,
/// which can get away with trimming trailing zeros only because printable
/// text never legitimately ends in one.
///
/// An out-of-order or skipped sequence number aborts that attempt
/// (freeing any frames already allocated for it) rather than silently
/// loading a corrupt tape, since this deliberately simple protocol has no
/// retransmission; waiting for a fresh start after an abort, or after any
/// unrelated sender's frame, costs nothing and needs no separate state.
#[inline(never)]
fn tape_load_net() {
    let Some(nic) = nic_or_report() else { return };
    let own_mac = nic.mac_address();
    println!("Waiting for a tape over the network; press F12 (or F4/F6/F7/F8/F10) to give up.");
    println!(
        "Have the sender target this card's MAC address ({}).",
        Mac(&own_mac)
    );

    let mut buffer = [0u8; nic::MAX_FRAME_LEN];
    let mut transfer: Option<TapeTransfer> = None;
    // Periodic (~2s) progress line while a transfer is in flight, so a
    // stalled load is visibly distinguishable from one still making
    // progress -- and, since `Rtl8139::overflow_count` only climbs when
    // the receive ring genuinely overflowed (see `Rtl8139::recover`
    // in rtl8139.rs), a nonzero and growing count here is a strong sign
    // the sender is outrunning this driver's polling loop, distinct from
    // a sender that has simply stopped, gone quiet, or was never reached
    // at all.
    let mut next_status = interrupts::ticks() + TAPE_STATUS_TICKS;

    loop {
        if matches!(
            keyboard::poll_term_event(),
            Some(keyboard::KeyEvent::Escape)
        ) {
            println!("\nGave up waiting; no tape loaded.");
            return;
        }
        if transfer.is_some() && interrupts::ticks() >= next_status {
            let active = transfer.as_ref().unwrap();
            println!(
                "\n... {} of {} byte(s) received so far ({} Rx overflow recover{}).",
                active.received,
                active.length,
                nic.overflow_count(),
                if nic.overflow_count() == 1 {
                    "y"
                } else {
                    "ies"
                }
            );
            next_status = interrupts::ticks() + TAPE_STATUS_TICKS;
        }
        let Some(received) = nic.receive(&mut buffer) else {
            continue;
        };
        let frame = &buffer[..received];
        // While this nested loop owns NIC polling, do not let a command
        // from the suspended remote terminal editor vanish into the tape
        // protocol's catch-all arm. A remote hangup still ends the session
        // once `term_net` resumes; normal data is replayed by that editor.
        if defer_active_term_input(frame) {
            if unsafe { PENDING_REMOTE_BYE } {
                println!("\nRemote terminal disconnected; no tape loaded.");
                return;
            }
            continue;
        }
        if frame.len() < 15 || frame[12..14] != CHAT_ETHERTYPE {
            continue;
        }
        let source: [u8; 6] = frame[6..12].try_into().unwrap();
        let payload = &frame[15..];

        match frame[14] {
            TAPE_TAG_START if transfer.is_none() => {
                let Some(length_bytes) = payload.get(0..4) else {
                    continue;
                };
                let length = u32::from_le_bytes(length_bytes.try_into().unwrap());
                if length == 0 {
                    println!("\nIgnoring a zero-length tape start from {}.", Mac(&source));
                    continue;
                }
                let Some(address) = allocate_tape_buffer(length) else {
                    continue;
                };
                println!("\nReceiving a {length}-byte tape from {}...", Mac(&source));
                transfer = Some(TapeTransfer {
                    source,
                    address,
                    length,
                    expected_seq: 0,
                    received: 0,
                });
            }
            TAPE_TAG_DATA
                if transfer
                    .as_ref()
                    .is_some_and(|transfer| transfer.source == source) =>
            {
                let active = transfer.as_mut().unwrap();
                let Some(seq_bytes) = payload.get(0..4) else {
                    continue;
                };
                let seq = u32::from_le_bytes(seq_bytes.try_into().unwrap());
                if seq != active.expected_seq {
                    println!(
                        "\nGap in tape data from {}: expected chunk #{}, got #{seq} -- aborting this load.",
                        Mac(&source),
                        active.expected_seq
                    );
                    transfer = None;
                    continue;
                }
                let offset = seq as usize * TAPE_CHUNK_LEN;
                let chunk_len = (active.length as usize - offset).min(TAPE_CHUNK_LEN);
                let data = &payload[4..];
                if data.len() < chunk_len {
                    println!(
                        "\nShort tape data chunk #{seq} from {}: expected {chunk_len} byte(s), frame only carried {} -- aborting this load.",
                        Mac(&source),
                        data.len()
                    );
                    transfer = None;
                    continue;
                }
                let destination = unsafe {
                    core::slice::from_raw_parts_mut(
                        (active.address as usize + offset) as *mut u8,
                        chunk_len,
                    )
                };
                destination.copy_from_slice(&data[..chunk_len]);
                active.received += chunk_len as u32;
                active.expected_seq += 1;
            }
            TAPE_TAG_STOP
                if transfer
                    .as_ref()
                    .is_some_and(|transfer| transfer.source == source) =>
            {
                let active = transfer.take().unwrap();
                if active.received == active.length {
                    if !install_tape(active.address, active.length) {
                        return;
                    }
                    println!(
                        "\nTape loaded: {} byte(s) at 0x{:05x} from {}. 'tape run' to execute it.",
                        active.length,
                        active.address,
                        Mac(&source)
                    );
                    return;
                }
                println!(
                    "\nTape from {} stopped early: only {} of {} declared byte(s) arrived -- aborting.",
                    Mac(&source),
                    active.received,
                    active.length
                );
            }
            CHAT_TAG_DISCOVER_REQUEST => {
                // A `send_tape.py`/`nic discover`/`chat` peer probing for
                // us: reply the same way `handle_incoming_chat_frame` and
                // `nic_discover` do, regardless of whether a transfer is
                // in progress, so this host stays discoverable the whole
                // time it sits in `tape load net`, not only before/after
                // a transfer.
                remember_peer(source);
                send_chat_control(nic, &source, &own_mac, CHAT_TAG_DISCOVER_REPLY);
                println!("\n(replied to a discovery probe from {})", Mac(&source));
            }
            _ => {} // A stray/unrelated tag, or traffic from a different sender mid-transfer: ignored.
        }
    }
}

fn tape_send_command(info: &BootInfo, arguments: &[u8]) {
    let (name, remainder) = take_word(arguments);
    let (peer_word, extra) = take_word(remainder);
    if name.is_empty() || peer_word.is_empty() || !extra.is_empty() {
        println!("Usage: tape send NAME PEER_MAC|PEER_NUMBER");
        return;
    }
    let destination = if let Some(mac) = parse_mac(peer_word) {
        mac
    } else if let Some(index) = parse_number(peer_word) {
        match peer_by_index(index as usize) {
            Some(mac) => mac,
            None => {
                println!(
                    "No peer #{index} known; run 'nic discover' first, or give a full MAC address."
                );
                return;
            }
        }
    } else {
        println!(
            "'{}' is neither a MAC address (aa:bb:cc:dd:ee:ff) nor a known peer",
            core::str::from_utf8(peer_word).unwrap_or("?")
        );
        println!("number; see 'nic peers'.");
        return;
    };

    let length = match fosfs::file_length(info, disk::Device::Boot, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length == 0 {
        println!(
            "'{}' is empty; nothing to send.",
            core::str::from_utf8(name).unwrap_or("?")
        );
        return;
    }
    if length > u32::MAX as u64 {
        println!("File is too large for this 32-bit tape transfer.");
        return;
    }

    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let data = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, disk::Device::Boot, name, data) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    if !install_tape(address, length as u32) {
        return;
    }
    let Some(nic) = nic_or_report() else { return };
    let source = nic.mac_address();
    tape_send(nic, &destination, &source, data);
}

/// Sends one tape (`TAPE_TAG_START`, then `TAPE_TAG_DATA` chunks, then
/// `TAPE_TAG_STOP`, see the `TAPE_TAG_*` constants) unicast to
/// `destination`, mirroring `tools/send_tape.py`'s host-side sender so
/// either end can talk to the other.
fn tape_send(nic: &nic::Nic, destination: &[u8; 6], source: &[u8; 6], data: &[u8]) {
    let mut start_frame = [0u8; 19];
    start_frame[0..6].copy_from_slice(destination);
    start_frame[6..12].copy_from_slice(source);
    start_frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    start_frame[14] = TAPE_TAG_START;
    start_frame[15..19].copy_from_slice(&(data.len() as u32).to_le_bytes());
    if !nic.send(&start_frame) {
        println!("Transmit failed (card reported an error, or timed out);");
        println!("tape not sent.");
        return;
    }
    pace_tape_send();

    let mut data_frame = [0u8; 15 + 4 + TAPE_CHUNK_LEN];
    data_frame[0..6].copy_from_slice(destination);
    data_frame[6..12].copy_from_slice(source);
    data_frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    data_frame[14] = TAPE_TAG_DATA;
    for (seq, chunk) in data.chunks(TAPE_CHUNK_LEN).enumerate() {
        data_frame[15..19].copy_from_slice(&(seq as u32).to_le_bytes());
        data_frame[19..19 + chunk.len()].copy_from_slice(chunk);
        if !nic.send(&data_frame[..19 + chunk.len()]) {
            println!(
                "Transmit failed on chunk #{seq} (card reported an error, or"
            );
            println!("timed out); tape not fully sent.");
            return;
        }
        pace_tape_send();
    }

    let mut stop_frame = [0u8; 15];
    stop_frame[0..6].copy_from_slice(destination);
    stop_frame[6..12].copy_from_slice(source);
    stop_frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    stop_frame[14] = TAPE_TAG_STOP;
    if !nic.send(&stop_frame) {
        println!("Transmit failed sending the closing frame; the receiver may");
        println!("report an incomplete tape.");
        return;
    }

    println!("Sent {} byte(s) to {}.", data.len(), Mac(destination));
}

fn pace_tape_send() {
    let start = interrupts::ticks();
    while interrupts::ticks().wrapping_sub(start) < TAPE_SEND_PACE_TICKS {
        core::hint::spin_loop();
    }
}

/// Displays a MAC address as `aa:bb:cc:dd:ee:ff` wherever `println!`/`print!`
/// need one, without repeating the six-field hex format at every call site.
struct Mac<'a>(&'a [u8; 6]);

impl core::fmt::Display for Mac<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            self.0[0], self.0[1], self.0[2], self.0[3], self.0[4], self.0[5]
        )
    }
}

/// Parses `aa:bb:cc:dd:ee:ff`-style text into a MAC address, mirroring
/// `tools/send_frame.py`'s `parse_mac` for consistency across the
/// host/kernel halves of the chat/sniff tooling.
fn parse_mac(text: &[u8]) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut fields = text.split(|&byte| byte == b':');
    for slot in mac.iter_mut() {
        let field = fields.next()?;
        if field.len() != 2 {
            return None;
        }
        *slot = u8::from_str_radix(core::str::from_utf8(field).ok()?, 16).ok()?;
    }
    if fields.next().is_some() {
        return None;
    }
    Some(mac)
}

fn no_arguments(arguments: &[u8]) -> bool {
    if arguments.is_empty() {
        true
    } else {
        println!("This command does not accept arguments.");
        false
    }
}

/// Parses a strict `diskN` token (`disk` followed by one or more decimal
/// digits) into a [`disk::Device`], but only when `N` names a disk that is
/// actually attached right now: `disk1` is always the boot disk; `disk2`,
/// `disk3`, ... map to [`disk::discover_secondary_disks`]'s results in
/// discovery order. Never guesses -- an out-of-range or malformed token
/// simply isn't recognized as a selector at all, so a FOSFS file literally
/// named e.g. `disk2` stays reachable unless a second disk really is
/// attached (see AGENTS.md's "A second (non-boot) disk" section).
fn strip_disk_selector(word: &[u8]) -> Option<disk::Device> {
    let suffix = word.strip_prefix(b"disk")?;
    if suffix.is_empty() || !suffix.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let number: u32 = core::str::from_utf8(suffix).ok()?.parse().ok()?;
    match number {
        0 => None,
        1 => Some(disk::Device::Boot),
        n => {
            let index = (n - 2) as usize;
            (index < disk::secondary_count()).then_some(disk::Device::Secondary(index))
        }
    }
}

/// Parses an optional `diskN` selector for a command that itself takes no
/// further arguments (`ls`, `format`, `diskinfo`): a lone leading `diskN`
/// word is unambiguous here, since none of these commands mean anything
/// else by their argument.
fn parse_disk_only(arguments: &[u8]) -> (disk::Device, &[u8]) {
    let (first, remainder) = take_word(arguments);
    match strip_disk_selector(first) {
        Some(device) => (device, remainder),
        None => (disk::Device::Boot, arguments),
    }
}

/// Parses an optional `diskN` selector for a command that takes exactly one
/// further argument (`cat NAME`, `rm NAME`, `readsector LBA`). A `diskN`-
/// shaped first word is only treated as a selector when something follows
/// it; with nothing left over, it must be the command's own single
/// argument instead -- e.g. `cat disk2` still reads a file literally named
/// `disk2` when there is nothing else on the line to read.
fn parse_disk_and_one_argument(arguments: &[u8]) -> (disk::Device, &[u8]) {
    let (first, remainder) = take_word(arguments);
    if remainder.is_empty() {
        return (disk::Device::Boot, arguments);
    }
    match strip_disk_selector(first) {
        Some(device) => (device, remainder),
        None => (disk::Device::Boot, arguments),
    }
}

/// Parses an optional `diskN` selector for `write NAME TEXT`, which takes
/// two further arguments. The same disambiguation rule applies one level
/// deeper: `diskN` is only a selector when a non-empty NAME *and* non-empty
/// TEXT both remain after it, so `write disk2 hello` still means "write a
/// file named disk2 containing 'hello'" rather than an invalid selector
/// with a missing TEXT.
fn parse_disk_and_two_arguments(arguments: &[u8]) -> (disk::Device, &[u8]) {
    let (first, remainder) = take_word(arguments);
    let (name, text) = take_word(remainder);
    if name.is_empty() || text.is_empty() {
        return (disk::Device::Boot, arguments);
    }
    match strip_disk_selector(first) {
        Some(device) => (device, remainder),
        None => (disk::Device::Boot, arguments),
    }
}

/// The human-facing `diskN` number for a [`disk::Device`], for status
/// messages (`disk1` is always the boot disk).
fn device_number(device: disk::Device) -> u32 {
    match device {
        disk::Device::Boot => 1,
        disk::Device::Secondary(index) => index as u32 + 2,
    }
}

fn take_word(input: &[u8]) -> (&[u8], &[u8]) {
    let input = trim_start(input);
    let end = input
        .iter()
        .position(|byte| *byte == b' ')
        .unwrap_or(input.len());
    (&input[..end], trim_start(&input[end..]))
}

fn trim_start(mut input: &[u8]) -> &[u8] {
    while input.first() == Some(&b' ') {
        input = &input[1..];
    }
    input
}

fn filesystem_result(result: Result<(), disk::Error>, success: &str) {
    match result {
        Ok(()) if !success.is_empty() => println!("{success}"),
        Ok(()) => {}
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

#[inline(never)]
fn cat_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: cat [diskN] NAME");
        return;
    }
    match fosfs::cat(info, device, name) {
        Ok(true) => {}
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

#[inline(never)]
fn write_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_two_arguments(arguments);
    let (name, text) = take_word(arguments);
    if name.is_empty() || text.is_empty() {
        println!("Usage: write [diskN] NAME TEXT");
        return;
    }
    match fosfs::write(info, device, name, text) {
        Ok(fosfs::WriteOutcome::Written) => println!("File written."),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

#[inline(never)]
fn remove_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: rm [diskN] NAME");
        return;
    }
    match fosfs::remove(info, device, name) {
        Ok(true) => println!("File removed."),
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// `copy SRC_DISK NAME DST_DISK [NEW_NAME]`. Unlike every other FOSFS
/// command, both disks here must be spelled out explicitly (even `disk1`)
/// rather than one side defaulting to the boot disk: with two disks
/// genuinely in play, a silent default would be ambiguous about which side
/// it applies to. Reuses the shared `pmem::TAPE_BASE` staging buffer that
/// `tape load disk` already uses for "read a whole file into memory, then
/// do something with it," rather than adding a second large static buffer;
/// see `copy_file`'s doc comment for why that is safe.
#[inline(never)]
fn copy_command(info: &BootInfo, arguments: &[u8]) {
    let (source_word, remainder) = take_word(arguments);
    let (name, remainder) = take_word(remainder);
    let (destination_word, remainder) = take_word(remainder);
    let (new_name, remainder) = take_word(remainder);
    if name.is_empty() || destination_word.is_empty() || !remainder.is_empty() {
        println!("Usage: copy SRC_DISK NAME DST_DISK [NEW_NAME]");
        return;
    }
    let Some(source) = strip_disk_selector(source_word) else {
        println!("Usage: copy SRC_DISK NAME DST_DISK [NEW_NAME]; SRC_DISK must name");
        println!("an attached disk (e.g. disk1).");
        return;
    };
    let Some(destination) = strip_disk_selector(destination_word) else {
        println!("Usage: copy SRC_DISK NAME DST_DISK [NEW_NAME]; DST_DISK must name");
        println!("an attached disk (e.g. disk2).");
        return;
    };
    let destination_name = if new_name.is_empty() { name } else { new_name };
    copy_file(info, source, name, destination, destination_name);
}

/// Reads a whole file from `source` into the shared tape staging buffer
/// (`pmem::TAPE_BASE`/`TAPE_LEN`, the same one `tape load disk`/`tape load
/// net` use) and writes it back out to `destination` under
/// `destination_name`. This is safe to share with the tape subsystem
/// because nothing here calls `install_tape`: a copy never becomes a
/// loaded tape, and any tape that *was* loaded there is invalidated
/// afterward (its bytes are now whatever file was just copied through),
/// exactly like loading a genuinely new tape already overwrites the
/// previous one in place.
fn copy_file(
    info: &BootInfo,
    source: disk::Device,
    name: &[u8],
    destination: disk::Device,
    destination_name: &[u8],
) {
    let length = match fosfs::file_length(info, source, name) {
        Ok(Some(length)) => length,
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    };
    if length > u32::MAX as u64 {
        println!("File is too large to copy through the shared tape-buffer scratch area.");
        return;
    }
    let length = length as usize;
    let Some(address) = allocate_tape_buffer(length as u32) else {
        return;
    };
    let buffer = unsafe { core::slice::from_raw_parts_mut(address as *mut u8, length) };
    match fosfs::read(info, source, name, buffer) {
        Ok(Some(_)) => {}
        Ok(None) => {
            println!("File not found.");
            return;
        }
        Err(error) => {
            println!("FOSFS disk error: {error:?}");
            return;
        }
    }
    let outcome = fosfs::write(info, destination, destination_name, buffer);
    invalidate_tape_after_scratch_use();
    match outcome {
        Ok(fosfs::WriteOutcome::Written) => println!(
            "Copied {length} byte(s) from disk{} '{}' to disk{} '{}'.",
            device_number(source),
            core::str::from_utf8(name).unwrap_or("?"),
            device_number(destination),
            core::str::from_utf8(destination_name).unwrap_or("?")
        ),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 lowercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::DirectoryFull) => {
            println!("Directory full: all 64 FOSFS entries are in use.")
        }
        Ok(fosfs::WriteOutcome::DataFull) => {
            println!("Data region full: no free sector remains in the FOSFS partition.")
        }
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

/// Clears any previously loaded tape's bookkeeping after `copy_file` reuses
/// the tape staging buffer as scratch space, and says so -- silently
/// leaving stale `TAPE`/`TAPE_ENTRY` state around would let a later `tape
/// run` misinterpret the just-copied file's bytes as tape code instead of
/// honestly reporting "nothing is loaded."
fn invalidate_tape_after_scratch_use() {
    let had_tape = tape_get().is_some();
    unsafe {
        *(&raw mut TAPE) = None;
    }
    if had_tape {
        println!("Note: this reused the shared tape buffer, so the previously");
        println!("loaded tape is gone; 'tape load' again before 'tape run'.");
    }
}

fn print_disk_info(info: &BootInfo, device: disk::Device) {
    println!(
        "disk{}: {} sectors; FOSFS partition: LBA {} + {} sectors",
        device_number(device),
        disk::device_sector_count(info, device),
        disk::device_data_partition_lba(info, device),
        disk::device_data_partition_sectors(info, device)
    );
    println!("Only FOSFS commands may write; raw sector inspection is read-only.");
    let secondary = disk::secondary_count();
    if device == disk::Device::Boot && secondary > 0 {
        println!(
            "{secondary} additional disk(s) attached: disk2..disk{}. Give diskinfo diskN for details.",
            secondary + 1
        );
    }
}

#[inline(never)]
fn read_sector_command(info: &BootInfo, arguments: &[u8]) {
    let (device, arguments) = parse_disk_and_one_argument(arguments);
    let (value, remainder) = take_word(arguments);
    let Some(lba) = parse_number(value) else {
        println!("Usage: readsector [diskN] LBA (decimal or 0x hexadecimal)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: readsector [diskN] LBA");
        return;
    }
    match disk::device_read_sector(info, device, lba as u64, |sector| {
        dump_bytes(lba * 512, sector)
    }) {
        Ok(()) => {}
        Err(error) => println!("Disk read failed: {error:?}"),
    }
}

#[inline(never)]
fn hexdump_command(info: &BootInfo, arguments: &[u8]) {
    let (address, remainder) = take_word(arguments);
    let (length, extra) = take_word(remainder);
    let (Some(address), Some(length)) = (parse_number(address), parse_number(length)) else {
        println!("Usage: hexdump ADDRESS LENGTH (kernel memory only; maximum 128 bytes)");
        return;
    };
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    if !extra.is_empty()
        || length == 0
        || length > 128
        || address < kernel_start
        || address
            .checked_add(length)
            .is_none_or(|end| end > kernel_end)
    {
        println!("Address must be within the reserved kernel range; maximum is 128 bytes.");
        return;
    }
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length as usize) };
    dump_bytes(address, bytes);
}

fn dump_bytes(base: u32, bytes: &[u8]) {
    for (row, chunk) in bytes.chunks(16).enumerate() {
        print!("0x{:08x}: ", base + (row * 16) as u32);
        for byte in chunk {
            print!("{byte:02x} ");
        }
        for _ in chunk.len()..16 {
            print!("   ");
        }
        print!("|");
        for byte in chunk {
            print!(
                "{}",
                if (0x20..=0x7e).contains(byte) {
                    *byte as char
                } else {
                    '.'
                }
            );
        }
        println!("|");
    }
}

fn parse_number(value: &[u8]) -> Option<u32> {
    let (radix, digits) = if value.starts_with(b"0x") {
        (16, &value[2..])
    } else {
        (10, value)
    };
    if digits.is_empty() {
        return None;
    }
    digits.iter().try_fold(0u32, |number, digit| {
        let value = match *digit {
            b'0'..=b'9' => u32::from(*digit - b'0'),
            b'a'..=b'f' if radix == 16 => u32::from(*digit - b'a' + 10),
            b'A'..=b'F' if radix == 16 => u32::from(*digit - b'A' + 10),
            _ => return None,
        };
        if value >= radix {
            return None;
        }
        number.checked_mul(radix)?.checked_add(value)
    })
}

fn print_boot_info(info: &BootInfo) {
    let boot_drive = info.boot_drive;
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    let serial_available = info.serial_available != 0;
    println!(
        "Boot drive: 0x{boot_drive:02x}; serial: {}; kernel: 0x{kernel_start:08x}-0x{kernel_end:08x}",
        if serial_available {
            "available"
        } else {
            "unavailable"
        }
    );
}

#[inline(never)]
fn print_memory_map(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    println!("Firmware memory map ({count} entries reported by BIOS INT 15h EAX=E820h):");
    for index in 0..count {
        let region =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        print_region(index, region);
    }
}

fn e820_type_name(kind: u32) -> &'static str {
    match kind {
        1 => "usable RAM",
        2 => "reserved (not usable)",
        3 => "ACPI reclaimable",
        4 => "ACPI NVS",
        5 => "bad memory",
        _ => "unknown type",
    }
}

fn print_memory_summary(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    let mut usable = 0u64;
    let mut reserved = 0u64;
    for index in 0..count {
        let region =
            unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        let length = read_u64(core::ptr::addr_of!(region.length));
        if read_u32(core::ptr::addr_of!(region.kind)) == 1 {
            usable = usable.saturating_add(length);
        } else {
            reserved = reserved.saturating_add(length);
        }
    }
    println!("E820 summary: {usable} usable bytes; {reserved} non-usable bytes");
    println!("(reserved/ACPI/bad).");
}

fn print_region(index: usize, region: MemoryRegion) {
    let base = read_u64(core::ptr::addr_of!(region.base));
    let length = read_u64(core::ptr::addr_of!(region.length));
    let kind = read_u32(core::ptr::addr_of!(region.kind));
    println!(
        "  {index:02}: 0x{base:016x} + 0x{length:016x} type {kind} ({})",
        e820_type_name(kind)
    );
}

fn read_u16(pointer: *const u16) -> u16 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

fn read_u32(pointer: *const u32) -> u32 {
    unsafe { core::ptr::read_unaligned(pointer) }
}

fn read_u64(pointer: *const u64) -> u64 {
    unsafe { core::ptr::read_unaligned(pointer) }
}
