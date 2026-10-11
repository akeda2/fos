//! Extra serial ports for tapes (`serial_*` in the tape ABI, version 7): the 16550 UARTs at COM2-COM4. COM1 is the console
//! (`serial.rs`) and is never offered. A terminal tape (`fterm`) is the user.
//!
//! Polled, like COM1: no interrupt. The 16550's receive FIFO holds 16 bytes (1.4 ms at 115200 baud), and a tape's loop can be
//! busy longer than that (a screen repaint), so `poll` moves the FIFO into a 4 KiB ring each time a tape calls into the
//! kernel for input, and `api_wait_for_event` yields for 100 us instead of `hlt` while a port is open. What does not fit the ring is
//! counted (`overruns`), never hidden. The USB serial adapters of later work will be another `kind` behind the same
//! functions; a tape does not see the difference.
//!
//! Registers (offset from the base): 0 data / divisor low (DLAB), 1 interrupt enable / divisor high, 2 FIFO control, 3 line control,
//! 4 modem control, 5 line status, 6 modem status, 7 scratch. A machine with no UART there reads 0xff everywhere, which the
//! detection (a scratch register that keeps what is written) turns down.

use fosfor_abi::tape::{
    SERIAL_BAD_SETTINGS, SERIAL_BUSY, SERIAL_KIND_UART, SERIAL_LINE_CTS, SERIAL_LINE_DCD, SERIAL_LINE_DSR, SERIAL_LINE_DTR, SERIAL_LINE_RI,
    SERIAL_LINE_RTS, SERIAL_NO_PORT, SerialInfo,
};

const BASES: [(u16, &[u8; 4]); 3] = [(0x2f8, b"COM2"), (0x3e8, b"COM3"), (0x2e8, b"COM4")];
const RING: usize = 4096;
/// The UART's clock divided by 16: baud = 115200 / divisor.
const CLOCK: u32 = 115_200;

struct Port {
    base: u16,
    name: [u8; 4],
    open: bool,
    baud: u32,
    format: u32,
    ring: [u8; RING],
    head: usize,
    tail: usize,
    rx_bytes: u64,
    tx_bytes: u64,
    overruns: u32,
    framing: u32,
    parity: u32,
    breaks: u32,
    /// Modem control register as the tape set it: DTR bit 0, RTS bit 1.
    modem: u8,
    /// Taken by the kernel as the shell's console (`serial-shell` in `settings`); tapes cannot open or read it.
    shell: bool,
}

static mut PORTS: [Option<Port>; 3] = [const { None }, const { None }, const { None }];
static mut PROBED: bool = false;

fn outb(port: u16, value: u8) {
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack)) };
}

fn inb(port: u16) -> u8 {
    let value: u8;
    unsafe { core::arch::asm!("in al, dx", in("dx") port, out("al") value, options(nomem, nostack)) };
    value
}

/// True if a 16550 answers at `base`: the scratch register keeps two different patterns, and the line status is not 0xff.
fn present(base: u16) -> bool {
    if inb(base + 5) == 0xff {
        return false;
    }
    outb(base + 7, 0xa5);
    if inb(base + 7) != 0xa5 {
        return false;
    }
    outb(base + 7, 0x5a);
    inb(base + 7) == 0x5a
}

fn ports() -> &'static mut [Option<Port>; 3] {
    unsafe {
        if !PROBED {
            PROBED = true;
            let table = &mut *core::ptr::addr_of_mut!(PORTS);
            let mut slot = 0;
            for (base, name) in BASES {
                if present(base) {
                    table[slot] = Some(Port {
                        base,
                        name: *name,
                        open: false,
                        baud: 0,
                        format: 0,
                        ring: [0; RING],
                        head: 0,
                        tail: 0,
                        rx_bytes: 0,
                        tx_bytes: 0,
                        overruns: 0,
                        framing: 0,
                        parity: 0,
                        breaks: 0,
                        modem: 0,
                        shell: false,
                    });
                    slot += 1;
                }
            }
        }
        &mut *core::ptr::addr_of_mut!(PORTS)
    }
}

fn port(number: usize) -> Option<&'static mut Port> {
    number.checked_sub(1).and_then(|index| ports().get_mut(index)).and_then(|slot| slot.as_mut())
}

/// How many ports there are (numbered from 1).
pub fn count() -> usize {
    ports().iter().flatten().count()
}

/// Whether a tape has any port open: the tape's wait must then poll instead of sleeping.
pub fn any_open() -> bool {
    unsafe { (*core::ptr::addr_of!(PORTS)).iter().flatten().any(|port| port.open) }
}

/// Moves one port's FIFO into its ring and counts line errors.
fn drain(port: &mut Port) {
    let mut budget = 64;
    while budget > 0 {
        let status = inb(port.base + 5);
        if status == 0xff || status & 1 == 0 {
            break;
        }
        if status & 0x02 != 0 {
            port.overruns += 1;
        }
        if status & 0x04 != 0 {
            port.parity += 1;
        }
        if status & 0x08 != 0 {
            port.framing += 1;
        }
        if status & 0x10 != 0 {
            port.breaks += 1;
        }
        let byte = inb(port.base);
        let next = (port.head + 1) % RING;
        if next == port.tail {
            port.overruns += 1; // the tape is not reading: drop the newest, as a full FIFO would
        } else {
            port.ring[port.head] = byte;
            port.head = next;
            port.rx_bytes += 1;
        }
        budget -= 1;
    }
}

/// Services every open port; cheap when none is open.
pub fn poll() {
    for port in ports().iter_mut().flatten() {
        if port.open {
            drain(port);
        }
    }
}

pub fn info(number: usize, out: &mut SerialInfo) -> bool {
    let Some(port) = port(number) else { return false };
    let mut name = [0u8; 8];
    name[..4].copy_from_slice(&port.name);
    *out = SerialInfo {
        kind: SERIAL_KIND_UART,
        open: u32::from(port.open),
        io_base: u32::from(port.base),
        baud: port.baud,
        format: port.format,
        rx_bytes: port.rx_bytes,
        tx_bytes: port.tx_bytes,
        overruns: port.overruns,
        framing_errors: port.framing,
        parity_errors: port.parity,
        breaks: port.breaks,
        name,
    };
    true
}

/// Programs the port: divisor, line format, FIFOs on and cleared, no interrupts, DTR and RTS up.
pub fn open(number: usize, baud: u32, format: u32) -> i32 {
    let Some(port) = port(number) else { return SERIAL_NO_PORT };
    if port.shell {
        return SERIAL_BUSY;
    }
    if baud == 0 || baud > CLOCK || format & !0x1f != 0 {
        return SERIAL_BAD_SETTINGS;
    }
    let divisor = (CLOCK / baud).max(1) as u16;
    let base = port.base;
    outb(base + 1, 0x00);
    outb(base + 3, 0x80); // DLAB on
    outb(base, divisor as u8);
    outb(base + 1, (divisor >> 8) as u8);
    outb(base + 3, format as u8); // line format, DLAB off, no break
    outb(base + 2, 0xc7); // FIFO on, cleared, 14-byte trigger (unused: we poll)
    port.modem = 0x03;
    outb(base + 4, port.modem);
    while inb(base + 5) & 1 != 0 {
        inb(base);
    }
    inb(base + 6);
    port.baud = CLOCK / u32::from(divisor);
    port.format = format;
    port.open = true;
    port.head = 0;
    port.tail = 0;
    (port.rx_bytes, port.tx_bytes, port.overruns, port.framing, port.parity, port.breaks) = (0, 0, 0, 0, 0, 0);
    0
}

pub fn close(number: usize) {
    if let Some(port) = port(number) {
        if port.open && !port.shell {
            port.open = false;
            port.modem = 0;
            outb(port.base + 4, 0);
            outb(port.base + 3, (port.format as u8) & 0x1f);
        }
    }
}

/// A tape that ended leaves nothing open.
pub fn close_all() {
    for number in 1..=3 {
        close(number);
    }
}

pub fn read(number: usize, out: &mut [u8]) -> usize {
    let Some(port) = port(number).filter(|port| port.open && !port.shell) else { return 0 };
    drain(port);
    let mut count = 0;
    while count < out.len() && port.tail != port.head {
        out[count] = port.ring[port.tail];
        port.tail = (port.tail + 1) % RING;
        count += 1;
    }
    count
}

/// Hands the transmitter bytes while its holding register says it is empty (a whole 16-byte FIFO at once); never waits.
pub fn write(number: usize, data: &[u8]) -> usize {
    let Some(port) = port(number).filter(|port| port.open && !port.shell) else { return 0 };
    if data.is_empty() || inb(port.base + 5) & 0x20 == 0 {
        return 0;
    }
    let count = data.len().min(16);
    for &byte in &data[..count] {
        outb(port.base, byte);
    }
    port.tx_bytes += count as u64;
    count
}

pub fn lines(number: usize) -> u32 {
    let Some(port) = port(number).filter(|port| port.open && !port.shell) else { return 0 };
    let status = inb(port.base + 6);
    let mut lines = 0;
    for (bit, line) in [(0x10, SERIAL_LINE_CTS), (0x20, SERIAL_LINE_DSR), (0x40, SERIAL_LINE_RI), (0x80, SERIAL_LINE_DCD)] {
        if status & bit != 0 {
            lines |= line;
        }
    }
    if port.modem & 1 != 0 {
        lines |= SERIAL_LINE_DTR;
    }
    if port.modem & 2 != 0 {
        lines |= SERIAL_LINE_RTS;
    }
    lines
}

pub fn set_lines(number: usize, lines: u32) {
    let Some(port) = port(number).filter(|port| port.open && !port.shell) else { return };
    port.modem = u8::from(lines & SERIAL_LINE_DTR != 0) | (u8::from(lines & SERIAL_LINE_RTS != 0) << 1);
    outb(port.base + 4, port.modem);
}

pub fn set_break(number: usize, on: bool) {
    let Some(port) = port(number).filter(|port| port.open && !port.shell) else { return };
    outb(port.base + 3, (port.format as u8 & 0x1f) | if on { 0x40 } else { 0 });
}

// ---- the shell on a serial port (`serial-shell` in the `settings` file) ----
//
// A machine with no screen or keyboard can be driven from a laptop over a null-modem cable or a USB serial adapter: every
// console line is copied to the port and every byte it receives is typed into the shell, like COM1 but on a port of the
// operator's choice. It is opt-in (nothing is open unless `settings` asks), because the port is a full shell with no password.
// It is polled like the rest of this file: the main loop spins for 100 us between passes instead of halting while this is on, so
// a pasted line does not overflow the 16-byte FIFO; the cost is an idle CPU that is no longer idle, which is why it is a setting.

static mut SHELL: Option<usize> = None;

/// Gives port `number` to the shell. 0 on success, else a `SERIAL_*` code.
pub fn open_shell(number: usize, baud: u32) -> i32 {
    let code = open(number, baud, 0x03);
    if code == 0 {
        if let Some(port) = port(number) {
            port.shell = true;
            unsafe { SHELL = Some(number) };
        }
    }
    code
}

pub fn shell_active() -> bool {
    unsafe { *core::ptr::addr_of!(SHELL) }.is_some()
}

/// The next typed byte from the shell port.
pub fn shell_read() -> Option<u8> {
    let port = port(unsafe { SHELL }?)?;
    drain(port);
    if port.tail == port.head {
        return None;
    }
    let byte = port.ring[port.tail];
    port.tail = (port.tail + 1) % RING;
    Some(byte)
}

/// Copies console text to the shell port (LF as CR LF). Each byte waits a bounded time for the transmitter, so a port with
/// nothing on the other end (or a stopped terminal) costs time but never hangs the kernel; what does not fit is dropped.
pub fn shell_write(text: &str) {
    let Some(number) = (unsafe { SHELL }) else { return };
    let Some(port) = port(number) else { return };
    let mut send = |byte: u8| {
        for _ in 0..20_000 {
            if inb(port.base + 5) & 0x20 != 0 {
                outb(port.base, byte);
                port.tx_bytes += 1;
                return;
            }
            core::hint::spin_loop();
        }
    };
    for byte in text.bytes() {
        if byte == b'\n' {
            send(b'\r');
        }
        send(byte);
    }
}
