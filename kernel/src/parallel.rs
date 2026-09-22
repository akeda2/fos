//! Legacy ISA parallel ("Centronics"/LPT) port support, for `shell::term_printer`/
//! `shell::print_command`. Scope is deliberately narrow, mirroring the
//! `console::detect_ports`/`Uart` precedent for legacy serial ports: no PCI
//! parallel-card discovery (see AGENTS.md's "printer port" design notes),
//! no bidirectional/EPP/ECP modes, just the original IBM PC "SPP" (Standard
//! Parallel Port) register set every LPT port has supported since the
//! original 5150.
//!
//! # Register layout
//!
//! Each port occupies three consecutive I/O addresses:
//! - `base + 0` (Data, write-only in SPP mode): the byte to print.
//! - `base + 1` (Status, read-only): printer-reported state.
//! - `base + 2` (Control, write-only): host-driven handshake lines.
//!
//! # Status/control bit polarity: real-hardware caveat
//!
//! Several status/control bits are, on the physical Centronics cable,
//! active-low signals (`nBusy`, `nAck`, `nError`) that the ISA bus buffer
//! may or may not present inverted back to software depending on exactly
//! which of several commonly (and inconsistently) documented conventions a
//! given reference follows -- this is the same class of surprise this
//! project already hit with the RTL8139's `TCR` interframe-gap field and
//! CR/RCR/TCR write order (see AGENTS.md's "Real hardware notes"). The
//! polarities below follow the most common description (matching Linux's
//! historic `parport_pc`/`lp` drivers), but have only been checked against
//! QEMU's `-parallel` emulation, not a real Centronics cable -- they are
//! **not yet confirmed against a real printer** (the user's Panasonic
//! KX-P1083 is the intended validation target; see AGENTS.md).
//!
//! A real-hardware report of a false "out of paper" status against that
//! printer (with paper loaded, printer online, its own paper-out light
//! off) remains unexplained. An earlier theory -- that the printer's
//! DIP-switch emulation mode (IBM Proprinter vs. Standard/Epson) changes
//! how the Paper-Out *status line* is driven -- does not hold up: emulation
//! mode governs escape-code interpretation for text formatting, not how
//! hardware drives Centronics status signals from a physical paper
//! sensor. `Printer::status()` (exposed via the `printer` shell command's
//! raw status/bit decode) exists specifically so the real register value
//! can be read directly from the affected hardware instead of continuing
//! to guess; if that shows `paper_out=1` with real paper loaded, the
//! `STATUS_PAPER_OUT` bit or its polarity should be corrected from that
//! evidence directly.
use crate::arch::x86::{inb, outb};

/// BIOS Data Area addresses of each legacy LPT port's base I/O address, in
/// index order LPT1..LPT4 -- the parallel-port counterpart of
/// `console::detect_ports`'s `0x400` lookup for COM1..COM4. The BIOS fills
/// these in at POST (0 if a port was not found).
const BDA_LPT_PORTS: *const u16 = 0x408 as *const u16;

/// Detects which of LPT1..LPT4 the BIOS found at boot, returning each
/// one's base I/O port address (or `None` if absent). ISA-only: unlike
/// `console::detect_pci_ports`, there is deliberately no PCI parallel-card
/// discovery (see AGENTS.md).
pub fn detect_ports() -> [Option<u16>; 4] {
    let mut ports = [None; 4];
    for (index, port) in ports.iter_mut().enumerate() {
        let base = unsafe { BDA_LPT_PORTS.add(index).read_volatile() };
        if base != 0 {
            *port = Some(base);
        }
    }
    ports
}

/// Status register bit for "printer busy" (see the module doc comment's
/// polarity caveat): read as 0 while the printer is busy processing the
/// last byte, 1 once it is ready for the next one.
const STATUS_BUSY: u8 = 0x80;
/// Status register bit for "out of paper": 1 while the printer reports no
/// paper loaded.
const STATUS_PAPER_OUT: u8 = 0x20;
/// Status register bit for "selected"/online: 1 while the printer is
/// powered on and selected (not in an offline/local state).
const STATUS_SELECT: u8 = 0x10;
/// Status register bit for a latched hardware fault (e.g. dot-matrix head
/// jam): 0 while an error condition is asserted.
const STATUS_ERROR: u8 = 0x08;

/// Control register bit that pulses the strobe line to latch a byte
/// already written to the Data register.
const CONTROL_STROBE: u8 = 0x01;
/// Baseline control-register value used between bytes: `nSelectIn`
/// asserted and `nInit` left in its normal (not held in reset) state, no
/// strobe pulse, IRQ/bidirectional bits left off since this driver only
/// ever polls status.
const CONTROL_IDLE: u8 = 0x0c;

/// Number of status-register polls `write_byte` performs, per attempt,
/// waiting for the printer to report "not busy" before giving up and
/// reporting a fault -- bounded, like every other polling loop in this
/// kernel, rather than an unbounded spin. `shell::term_printer`/
/// `shell::print_command` are the ones that turn a reported fault into an
/// indefinite "pause and retry" from the *caller's* side (see their own
/// doc comments), so no single call here ever blocks forever.
const BUSY_POLL_ATTEMPTS: u32 = 200_000;

/// Why `Printer::write_byte` did not deliver a byte, so callers can report
/// (and keep retrying past) the specific real-world condition rather than
/// a bare failure.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PrinterFault {
    /// The printer is off, disconnected, or set to local/offline mode.
    NotSelected,
    /// The printer reports no paper loaded.
    PaperOut,
    /// The printer latched a hardware error condition.
    ErrorLine,
    /// The printer never reported "not busy" within `BUSY_POLL_ATTEMPTS`
    /// polls -- likely still working through a slow mechanical print head,
    /// not necessarily a fault; the caller should simply try again.
    Busy,
}

/// One legacy SPP parallel port. Write-only: unlike `console::Uart`, there
/// is no data channel back from the printer, only the status bits above.
pub struct Printer {
    base: u16,
}

impl Printer {
    pub const fn new(base: u16) -> Self {
        Printer { base }
    }

    fn data_port(&self) -> u16 {
        self.base
    }

    fn status_port(&self) -> u16 {
        self.base + 1
    }

    fn control_port(&self) -> u16 {
        self.base + 2
    }

    /// Reads the raw status register, for `shell`'s `printer` listing
    /// command.
    pub fn status(&self) -> u8 {
        unsafe { inb(self.status_port()) }
    }

    /// Classifies the current status register into a fault, or `None` if
    /// the printer looks ready to accept a byte right now.
    fn current_fault(&self) -> Option<PrinterFault> {
        let status = self.status();
        if status & STATUS_SELECT == 0 {
            Some(PrinterFault::NotSelected)
        } else if status & STATUS_PAPER_OUT != 0 {
            Some(PrinterFault::PaperOut)
        } else if status & STATUS_ERROR == 0 {
            Some(PrinterFault::ErrorLine)
        } else {
            None
        }
    }

    /// Writes and strobes one byte. Returns `Ok(())` once the printer has
    /// accepted it (observed busy, then not-busy again); returns a
    /// [`PrinterFault`] otherwise without blocking indefinitely -- callers
    /// wanting "pause and retry until it clears" (see `shell::
    /// term_printer`) simply call this again with the same byte.
    pub fn write_byte(&self, byte: u8) -> Result<(), PrinterFault> {
        if let Some(fault) = self.current_fault() {
            return Err(fault);
        }
        unsafe {
            outb(self.data_port(), byte);
            outb(self.control_port(), CONTROL_IDLE | CONTROL_STROBE);
            // A real Centronics strobe pulse is on the order of a
            // microsecond; a handful of port reads is a cheap, portable
            // way to hold it briefly without a dedicated delay primitive
            // (the same `io_wait`-style trick `interrupts::program_pic`
            // already uses for chipset command timing).
            for _ in 0..4 {
                inb(self.status_port());
            }
            outb(self.control_port(), CONTROL_IDLE);

            let mut attempts = BUSY_POLL_ATTEMPTS;
            while attempts != 0 {
                if let Some(fault) = self.current_fault() {
                    return Err(fault);
                }
                if inb(self.status_port()) & STATUS_BUSY != 0 {
                    return Ok(());
                }
                attempts -= 1;
            }
        }
        Err(PrinterFault::Busy)
    }
}
