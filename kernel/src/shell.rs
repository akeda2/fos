use crate::arch::x86::halt_forever;
use crate::boot_info::{BootInfo, MemoryRegion};
use crate::console;
use crate::cpu;
use crate::disk;
use crate::fosfs;
use crate::interrupts;
use crate::keyboard;
use crate::keyboard::Keyboard;
use crate::pmem;
use crate::rtl8139;
use crate::{print, println};

const LINE_CAPACITY: usize = 80;

pub fn run(info: &BootInfo) -> ! {
    let keyboard_available = Keyboard::initialize().is_some();
    interrupts::initialize();
    if keyboard_available {
        println!("PS/2 keyboard initialized. Type 'help' for commands.");
    } else {
        println!("PS/2 keyboard unavailable; serial input remains available.");
        println!("Type 'help' for commands.");
    }
    let mut line = [0u8; LINE_CAPACITY];
    let mut length = 0;
    let mut saw_carriage_return = false;
    prompt();

    loop {
        let byte = console::read_serial_byte().or_else(keyboard::pop_char);
        let Some(byte) = byte else {
            continue;
        };
        match byte {
            b'\n' if saw_carriage_return => saw_carriage_return = false,
            b'\r' | b'\n' => {
                println!();
                execute(&line[..length], info);
                length = 0;
                saw_carriage_return = byte == b'\r';
                prompt();
            }
            8 | 127 if length > 0 => {
                saw_carriage_return = false;
                length -= 1;
                console::erase_last_character();
            }
            0x20..=0x7e if length < LINE_CAPACITY => {
                saw_carriage_return = false;
                line[length] = byte;
                length += 1;
                print!("{}", byte as char);
            }
            0x20..=0x7e => {
                saw_carriage_return = false;
                println!("\nInput limit is {LINE_CAPACITY} characters.")
            }
            _ => {}
        }
    }
}

fn prompt() {
    print!("fos> ");
}

fn execute(command: &[u8], info: &BootInfo) {
    let (name, arguments) = take_word(command);
    match name {
        b"" => {}
        b"help" => {
            if no_arguments(arguments) {
                println!("System: help clear about bootinfo memmap memory cpuinfo hexdump halt");
                println!("Interrupts: ticks uptime irqstat pmem palloc pfree");
                println!("Disk: diskinfo readsector format ls cat write rm");
                println!("Serial: serial (list ports), term [PORT [BAUD]] (raw terminal; F12 to exit)");
                println!("Ethernet: nic (status), nic send (broadcast test frame), nic recv (poll for one frame),");
        println!("          nic sniff (promiscuous capture; F12 to stop)");
        println!("          chat [PEER_MAC] (broadcast or unicast text chat; F12 or /quit to leave)");
            }
        }
        b"clear" if no_arguments(arguments) => console::clear(),
        b"about" if no_arguments(arguments) => println!("fos is a small educational 32-bit x86 operating system."),
        b"bootinfo" if no_arguments(arguments) => print_boot_info(info),
        b"memmap" if no_arguments(arguments) => print_memory_map(info),
        b"memory" if no_arguments(arguments) => print_memory_summary(info),
        b"cpuinfo" if no_arguments(arguments) => print_cpu_info(),
        b"diskinfo" if no_arguments(arguments) => print_disk_info(info),
        b"ticks" if no_arguments(arguments) => print_ticks(),
        b"uptime" if no_arguments(arguments) => print_uptime(),
        b"irqstat" if no_arguments(arguments) => print_irq_stats(),
        b"pmem" if no_arguments(arguments) => print_pmem_stats(),
        b"palloc" if no_arguments(arguments) => palloc_command(),
        b"pfree" => pfree_command(arguments),
        b"readsector" => read_sector_command(info, arguments),
        b"hexdump" => hexdump_command(info, arguments),
        b"format" if no_arguments(arguments) => filesystem_result(fosfs::format(info), "FOSFS formatted."),
        b"ls" if no_arguments(arguments) => filesystem_result(fosfs::list(info), ""),
        b"cat" => cat_command(info, arguments),
        b"write" => write_command(info, arguments),
        b"rm" => remove_command(info, arguments),
        b"term" => term_command(arguments),
        b"serial" if no_arguments(arguments) => print_serial_ports(),
        b"nic" => nic_command(arguments),
        b"chat" => chat_command(arguments),
        b"halt" => {
            if !no_arguments(arguments) {
                return;
            }
            println!("Halting CPU. Reset or power-cycle to restart.");
            halt_forever();
        }
        b"clear" | b"about" | b"bootinfo" | b"memmap" | b"memory" | b"cpuinfo" | b"diskinfo" | b"format" | b"ls"
        | b"ticks" | b"uptime" | b"irqstat" | b"pmem" | b"palloc" | b"serial" => {
            println!("This command does not accept arguments.");
        }

        _ => println!("Unknown command. Type 'help' for commands."),
    }
}

fn print_ticks() {
    println!("Timer ticks: {} (at {} Hz)", interrupts::ticks(), interrupts::tick_frequency_hz());
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
    println!(
        "IRQ0 (timer): {} fired; IRQ1 (keyboard): {} fired",
        interrupts::timer_irq_count(),
        interrupts::keyboard_irq_count()
    );
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

fn print_serial_ports() {
    let ports = console::detect_ports();
    for (index, name) in SERIAL_PORT_NAMES.iter().enumerate() {
        let name = core::str::from_utf8(name).unwrap_or("?");
        match ports[index] {
            Some(base) if index == 0 => {
                println!("{name}: 0x{base:04x} (local shell console, fixed at 115200 8N1)");
            }
            Some(base) => println!("{name}: 0x{base:04x} (free for 'term'; baud chosen when entering it)"),
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
            println!("{name}: 0x{base:04x} (found via PCI enumeration; free for 'term'; baud chosen when entering it)");
        }
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
/// remains the way to invoke `term` and to leave it again with F12
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
fn term_command(arguments: &[u8]) {
    const USAGE: &str = "Usage: term [COM1|COM2|COM3|COM4|PCI0|PCI1|...] [BAUD]";

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

    if port_word.is_empty() || SERIAL_PORT_NAMES[0].eq_ignore_ascii_case(port_word) {
        if baud.is_some() {
            println!("COM1's baud rate is fixed at 115200 (it is shared with the local shell");
            println!("console, so changing it mid-session would desync anyone attached to it).");
            println!("Use COM2, COM3, COM4, or a PCI port for a custom baud rate.");
            return;
        }
        println!("Entering raw terminal mode on COM1 at 115200 baud: your keystrokes are sent");
        println!("byte-for-byte, with no local echo or line editing -- the far end echoes them");
        println!("back. Press F12 to return to the fos shell. If you are controlling fos over");
        println!("this same serial link, note that link is about to become the remote");
        println!("connection instead: use the VGA console to get back in that case.");
        term_loop(console::write_serial_byte, console::read_serial_byte);
        println!("\nLeft terminal mode; back at the fos shell.");
        return;
    }

    if let Some(index) = SERIAL_PORT_NAMES.iter().position(|name| name.eq_ignore_ascii_case(port_word)) {
        let ports = console::detect_ports();
        let Some(base) = ports[index] else {
            let name = core::str::from_utf8(SERIAL_PORT_NAMES[index]).unwrap_or("?");
            println!("{name} was not detected by the BIOS at boot; see 'serial'.");
            return;
        };
        run_uart_term(SERIAL_PORT_NAMES[index], base, baud);
        return;
    }

    if let Some(index) = PCI_PORT_NAMES.iter().position(|name| name.eq_ignore_ascii_case(port_word)) {
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
    println!("back. Press F12 to return to the fos shell.");
    term_loop(|byte| uart.write_byte(byte), || uart.read_byte());
    println!("\nLeft terminal mode; back at the fos shell.");
}

/// Shared by every `term` port: forwards keystrokes via `write` until F12
/// is pressed, and shows bytes arriving via `read` locally (VGA only, so
/// they are never re-echoed back over the same wire they arrived on).
fn term_loop(write: impl Fn(u8), read: impl Fn() -> Option<u8>) {
    loop {
        match keyboard::poll_term_event() {
            Some(keyboard::KeyEvent::Escape) => break,
            Some(keyboard::KeyEvent::Char(byte)) => write(byte),
            None => {}
        }
        if let Some(byte) = read() {
            console::write_vga_only(&[byte]);
        }
    }
}

/// A test-only ethertype from the "experimental" range (`0x88b5`/`0x88b6`,
/// reserved by IEEE 802 for exactly this purpose: local, non-routable
/// protocol experiments that will never collide with a real assigned
/// ethertype), used to mark `nic send`'s frames as fos's own test traffic
/// rather than anything meaningful to a real network.
const TEST_ETHERTYPE: [u8; 2] = [0x88, 0xb5];
const TEST_PAYLOAD: &[u8] = b"fos raw ethernet test frame";

/// `nic` (no arguments) reports whether an RTL8139 was found and brought
/// up, along with its MAC address; `nic send` transmits one broadcast
/// test frame (see `TEST_ETHERTYPE`); `nic recv` polls briefly for one
/// incoming frame and hex-dumps it if one arrives; `nic sniff` runs a
/// continuous promiscuous-mode capture, printing every frame seen until
/// F12 stops it. There is no protocol above raw Ethernet here (no ARP,
/// no IPv4) -- this only exercises the card itself.
fn nic_command(arguments: &[u8]) {
    let (subcommand, remainder) = take_word(arguments);
    if !remainder.is_empty() {
        println!("Usage: nic [send|recv|sniff]");
        return;
    }
    match subcommand {
        b"" => nic_status(),
        b"send" => nic_send(),
        b"recv" => nic_recv(),
        b"sniff" => nic_sniff(),
        _ => println!("Usage: nic [send|recv|sniff]"),
    }
}

fn nic_status() {
    match rtl8139::get() {
        Ok(nic) => {
            let mac = nic.mac_address();
            println!(
                "RTL8139 found: MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
            );
        }
        Err(rtl8139::Status::NotPresent) => println!("No RTL8139 Ethernet controller found on the PCI bus."),
        Err(rtl8139::Status::InitFailed(error)) => println!("RTL8139 found but could not be initialized: {error:?}"),
    }
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
        println!("Sent a {}-byte broadcast test frame (ethertype 0x{:02x}{:02x}).", frame.len(), TEST_ETHERTYPE[0], TEST_ETHERTYPE[1]);
    } else {
        println!("Transmit failed (card reported an error, or timed out).");
    }
}

/// Shared by every `nic` subcommand that needs the card itself (`send`,
/// `recv`, `sniff`): reports either failure case the same way `nic`
/// (status) does, and returns `None` so callers can just early-return.
fn nic_or_report() -> Option<&'static rtl8139::Rtl8139> {
    match rtl8139::get() {
        Ok(nic) => Some(nic),
        Err(rtl8139::Status::NotPresent) => {
            println!("No RTL8139 Ethernet controller found on the PCI bus.");
            None
        }
        Err(rtl8139::Status::InitFailed(error)) => {
            println!("RTL8139 found but could not be initialized: {error:?}");
            None
        }
    }
}

fn nic_recv() {
    let Some(nic) = nic_or_report() else { return };
    let mut buffer = [0u8; rtl8139::MAX_FRAME_LEN];
    let mut attempts = 20_000_000u32;
    loop {
        if let Some(length) = nic.receive(&mut buffer) {
            let frame = &buffer[..length];
            if length >= 14 {
                println!(
                    "Received {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    frame[0], frame[1], frame[2], frame[3], frame[4], frame[5],
                    frame[6], frame[7], frame[8], frame[9], frame[10], frame[11],
                    frame[12], frame[13]
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
/// until F12 stops the capture, the same escape key `term` uses and for
/// the same reason (layout-independent, unlike a Shift/AltGr combination
/// on a Nordic keyboard). Promiscuous mode is always turned back off
/// before returning, whether the capture ran to completion or not, so a
/// later `nic send`/`nic recv` is not left seeing unrelated traffic.
fn nic_sniff() {
    let Some(nic) = nic_or_report() else { return };

    println!("Enabling promiscuous mode: the card will now hand over every frame it sees");
    println!("on the wire, not just frames addressed to it (unicast/multicast/broadcast).");
    println!("Press F12 to stop capturing and return to the fos shell.");
    nic.set_promiscuous(true);

    let mut buffer = [0u8; rtl8139::MAX_FRAME_LEN];
    let mut captured = 0u32;
    loop {
        if matches!(keyboard::poll_term_event(), Some(keyboard::KeyEvent::Escape)) {
            break;
        }
        if let Some(length) = nic.receive(&mut buffer) {
            captured += 1;
            let frame = &buffer[..length];
            if length >= 14 {
                println!(
                    "#{captured} {length} bytes: dst {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} src {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ethertype 0x{:02x}{:02x}",
                    frame[0], frame[1], frame[2], frame[3], frame[4], frame[5],
                    frame[6], frame[7], frame[8], frame[9], frame[10], frame[11],
                    frame[12], frame[13]
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

/// A dedicated ethertype for `chat`, distinct from `TEST_ETHERTYPE`, so a
/// `nic sniff` capture (or a stray `nic recv`) never mistakes a chat line
/// for test traffic or vice versa. `0x88b5`/`0x88b6` are the two ethertypes
/// IEEE 802 reserves together for exactly this kind of local, non-routable
/// protocol experiment.
const CHAT_ETHERTYPE: [u8; 2] = [0x88, 0xb6];
const CHAT_TEXT_CAPACITY: usize = 200;
const BROADCAST_MAC: [u8; 6] = [0xff; 6];

/// `chat` (no arguments) is an open, broadcast text chat: every frame it
/// sends goes to the broadcast address, and it displays any `CHAT_ETHERTYPE`
/// frame it receives from anyone. `chat AA:BB:CC:DD:EE:FF` instead targets
/// one peer directly (a real unicast Ethernet address, not a broadcast) and
/// only displays frames whose *source* matches that address, for a private
/// two-machine conversation -- both send and receive reuse the exact same
/// `Rtl8139::send`/`receive` calls either way, since addressing is just a
/// different destination MAC, not a different protocol.
///
/// Typed lines are sent on Enter, one line per frame, with the same
/// backspace/echo/length-limit handling the main shell prompt itself uses;
/// incoming lines are printed as they arrive, tagged with the sender's MAC.
/// Because incoming lines can arrive while a local line is only half typed,
/// they may interleave with it on screen -- a known, accepted simplification
/// for a single-scrolling-console chat, not a bug. Leaves through F12 (only
/// available from the local PS/2 keyboard, same as `term`/`nic sniff`) or by
/// typing the line `/quit` (which also works from a serial-only session,
/// since -- unlike `term` -- `chat` never takes over the serial console).
fn chat_command(arguments: &[u8]) {
    let (peer_word, remainder) = take_word(arguments);
    if !remainder.is_empty() {
        println!("Usage: chat [PEER_MAC]");
        return;
    }
    let peer = if peer_word.is_empty() {
        None
    } else {
        match parse_mac(peer_word) {
            Some(mac) => Some(mac),
            None => {
                println!("'{}' is not a MAC address in aa:bb:cc:dd:ee:ff form.", core::str::from_utf8(peer_word).unwrap_or("?"));
                return;
            }
        }
    };

    let Some(nic) = nic_or_report() else { return };
    let destination = peer.unwrap_or(BROADCAST_MAC);
    let source = nic.mac_address();

    match peer {
        Some(mac) => println!(
            "Chatting with {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} only. F12 or '/quit' to leave.",
            mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
        ),
        None => println!("Broadcasting to everyone listening. F12 or '/quit' to leave."),
    }

    let mut line = [0u8; CHAT_TEXT_CAPACITY];
    let mut length = 0usize;
    let mut saw_carriage_return = false;
    let mut recv_buffer = [0u8; rtl8139::MAX_FRAME_LEN];

    'chat: loop {
        // Serial bytes take the same path a plain keyboard character
        // would (there is no serial equivalent of the F12 escape key);
        // only fall back to polling the keyboard -- which can also report
        // Escape -- once no serial byte is waiting.
        let event = match console::read_serial_byte() {
            Some(byte) => Some(keyboard::KeyEvent::Char(byte)),
            None => keyboard::poll_term_event(),
        };
        match event {
            Some(keyboard::KeyEvent::Escape) => break 'chat,
            Some(keyboard::KeyEvent::Char(b'\n')) if saw_carriage_return => saw_carriage_return = false,
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
            print_chat_frame(&recv_buffer[..received], peer.as_ref());
        }
    }

    println!("Left chat; back at the fos shell.");
}

fn send_chat_line(nic: &rtl8139::Rtl8139, destination: &[u8; 6], source: &[u8; 6], text: &[u8]) {
    let mut frame = [0u8; 14 + CHAT_TEXT_CAPACITY];
    frame[0..6].copy_from_slice(destination);
    frame[6..12].copy_from_slice(source);
    frame[12..14].copy_from_slice(&CHAT_ETHERTYPE);
    frame[14..14 + text.len()].copy_from_slice(text);
    if !nic.send(&frame[..14 + text.len()]) {
        println!("(message not sent: card reported an error, or timed out)");
    }
}

/// Prints one received frame if it is a `chat` line meant for us: the right
/// ethertype, and -- when in a unicast conversation (`peer` is `Some`) --
/// from that exact peer (broadcast-mode `chat` shows lines from anyone).
/// Trailing zero bytes are trimmed before decoding as text, since short
/// lines are zero-padded up to Ethernet's 60-byte minimum frame length the
/// same way `Rtl8139::send`/`tools/send_frame.py` both already do.
fn print_chat_frame(frame: &[u8], peer: Option<&[u8; 6]>) {
    if frame.len() < 14 || frame[12..14] != CHAT_ETHERTYPE {
        return;
    }
    let source = &frame[6..12];
    if let Some(peer) = peer {
        if source != peer {
            return;
        }
    }
    let payload = &frame[14..];
    let text_end = payload.iter().rposition(|&byte| byte != 0).map_or(0, |index| index + 1);
    let text = core::str::from_utf8(&payload[..text_end]).unwrap_or("<non-UTF-8 chat data>");
    println!(
        "\n{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}: {text}",
        source[0], source[1], source[2], source[3], source[4], source[5]
    );
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

fn take_word(input: &[u8]) -> (&[u8], &[u8]) {
    let input = trim_start(input);
    let end = input.iter().position(|byte| *byte == b' ').unwrap_or(input.len());
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

fn cat_command(info: &BootInfo, arguments: &[u8]) {
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: cat NAME");
        return;
    }
    match fosfs::cat(info, name) {
        Ok(true) => {}
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn write_command(info: &BootInfo, arguments: &[u8]) {
    let (name, text) = take_word(arguments);
    if name.is_empty() || text.is_empty() {
        println!("Usage: write NAME TEXT");
        return;
    }
    match fosfs::write(info, name, text) {
        Ok(fosfs::WriteOutcome::Written) => println!("File written."),
        Ok(fosfs::WriteOutcome::InvalidName) => {
            println!("Invalid name: use up to 16 uppercase letters, digits, '.', '_', or '-'.");
        }
        Ok(fosfs::WriteOutcome::ContentTooLarge) => println!("Content too large: FOSFS files are limited to one 512-byte sector."),
        Ok(fosfs::WriteOutcome::DirectoryFull) => println!("Directory full: all 64 FOSFS entries are in use."),
        Ok(fosfs::WriteOutcome::DataFull) => println!("Data region full: no free sector remains in the FOSFS partition."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn remove_command(info: &BootInfo, arguments: &[u8]) {
    let (name, remainder) = take_word(arguments);
    if name.is_empty() || !remainder.is_empty() {
        println!("Usage: rm NAME");
        return;
    }
    match fosfs::remove(info, name) {
        Ok(true) => println!("File removed."),
        Ok(false) => println!("File not found."),
        Err(error) => println!("FOSFS disk error: {error:?}"),
    }
}

fn print_disk_info(info: &BootInfo) {
    println!(
        "Disk: {} sectors; FOSFS partition: LBA {} + {} sectors",
        disk::sector_count(info),
        disk::data_partition_lba(info),
        disk::data_partition_sectors(info)
    );
    println!("Only FOSFS commands may write; raw sector inspection is read-only.");
}

fn read_sector_command(info: &BootInfo, arguments: &[u8]) {
    let (value, remainder) = take_word(arguments);
    let Some(lba) = parse_number(value) else {
        println!("Usage: readsector LBA (decimal or 0x hexadecimal)");
        return;
    };
    if !remainder.is_empty() {
        println!("Usage: readsector LBA");
        return;
    }
    match disk::read_sector(info, lba, |sector| dump_bytes(lba * 512, sector)) {
        Ok(()) => {}
        Err(error) => println!("Disk read failed: {error:?}"),
    }
}

fn hexdump_command(info: &BootInfo, arguments: &[u8]) {
    let (address, remainder) = take_word(arguments);
    let (length, extra) = take_word(remainder);
    let (Some(address), Some(length)) = (parse_number(address), parse_number(length)) else {
        println!("Usage: hexdump ADDRESS LENGTH (kernel memory only; maximum 128 bytes)");
        return;
    };
    let kernel_start = read_u32(core::ptr::addr_of!(info.kernel_start));
    let kernel_end = read_u32(core::ptr::addr_of!(info.kernel_end));
    if !extra.is_empty() || length == 0 || length > 128 || address < kernel_start || address.checked_add(length).is_none_or(|end| end > kernel_end) {
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
            print!("{}", if (0x20..=0x7e).contains(byte) { *byte as char } else { '.' });
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
        if serial_available { "available" } else { "unavailable" }
    );
}

fn print_memory_map(info: &BootInfo) {
    let count = usize::from(read_u16(core::ptr::addr_of!(info.memory_region_count)));
    println!("Firmware memory map ({count} entries reported by BIOS INT 15h EAX=E820h):");
    for index in 0..count {
        let region = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
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
        let region = unsafe { core::ptr::read_unaligned(core::ptr::addr_of!(info.memory_regions[index])) };
        let length = read_u64(core::ptr::addr_of!(region.length));
        if read_u32(core::ptr::addr_of!(region.kind)) == 1 {
            usable = usable.saturating_add(length);
        } else {
            reserved = reserved.saturating_add(length);
        }
    }
    println!("E820 summary: {usable} usable bytes; {reserved} non-usable bytes (reserved/ACPI/bad).");
}

fn print_region(index: usize, region: MemoryRegion) {
    let base = read_u64(core::ptr::addr_of!(region.base));
    let length = read_u64(core::ptr::addr_of!(region.length));
    let kind = read_u32(core::ptr::addr_of!(region.kind));
    println!("  {index:02}: 0x{base:016x} + 0x{length:016x} type {kind} ({})", e820_type_name(kind));
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
