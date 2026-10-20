//! The ARM PrimeCell PL011 UART: the Pi 4's UART0 and QEMU `virt`'s console. Transmit and polled receive.
//! Registers (offsets): DR 0x00 (write a byte), FR 0x18 (bit 5 TXFF: transmit FIFO full, bit 3 BUSY).
//! The firmware has already set the baud rate and enabled the UART, so the kernel just writes; a bounded wait on TXFF
//! keeps a UART that never drains from hanging the kernel.

use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// QEMU `virt`'s PL011. A real board's address is found from the firmware's SPCR/DT tables (the Pi 4's UART0 is
/// 0xFE201000 behind the firmware's own mapping); `acpi::spcr_uart` supplies it at boot (`set_base`).
pub const QEMU_VIRT_BASE: u64 = 0x0900_0000;

static BASE: AtomicU64 = AtomicU64::new(QEMU_VIRT_BASE);

pub fn base() -> u64 {
    BASE.load(Ordering::Relaxed)
}

pub fn set_base(base: u64) {
    BASE.store(base, Ordering::Relaxed);
}

fn put(byte: u8) {
    let base = BASE.load(Ordering::Relaxed);
    for _ in 0..100_000 {
        if unsafe { core::ptr::read_volatile((base + 0x18) as *const u32) } & 1 << 5 == 0 {
            break;
        }
    }
    unsafe { core::ptr::write_volatile(base as *mut u32, u32::from(byte)) };
}

/// One byte as is (the console writer turns `\n` into CRLF; a binary protocol must not).
pub fn write_raw(byte: u8) {
    put(byte);
}

/// The firmware's divisors (IBRD << 32 | FBRD) from before the first `set_baud`, to restore them.
static ORIGINAL: AtomicU64 = AtomicU64::new(0);

fn register(offset: u64) -> *mut u32 {
    (BASE.load(Ordering::Relaxed) + offset) as *mut u32
}

/// Switches the line speed (`baud` 0 restores the firmware's). The UART's input clock is not known, so it is worked out
/// from the divisors the firmware programmed for 115200 (the Pi 4's is 48 MHz); QEMU ignores the speed altogether.
/// Waits for the transmitter to finish, disables the UART while IBRD/FBRD change (LCRH must be written to latch them) and
/// empties the receive FIFO. Returns false for a speed the divisors cannot express.
pub fn set_baud(baud: u32) -> bool {
    unsafe {
        let (ibrd, fbrd) = (register(0x24).read_volatile() & 0xffff, register(0x28).read_volatile() & 0x3f);
        if ORIGINAL.load(Ordering::Relaxed) == 0 {
            ORIGINAL.store(u64::from(ibrd) << 32 | u64::from(fbrd), Ordering::Relaxed);
        }
        let original = ORIGINAL.load(Ordering::Relaxed);
        let (new_ibrd, new_fbrd) = if baud == 0 {
            ((original >> 32) as u32, original as u32)
        } else {
            let clock = if original >> 32 != 0 { 115_200 * ((original >> 32) * 64 + (original & 0x3f)) / 4 } else { 48_000_000 };
            let divisor = (clock * 4 + u64::from(baud) / 2) / u64::from(baud); // 64 x clock / (16 x baud)
            if divisor < 64 || divisor >> 6 > 0xffff {
                return false;
            }
            ((divisor >> 6) as u32, (divisor & 0x3f) as u32)
        };
        for _ in 0..1_000_000 {
            if register(0x18).read_volatile() & 1 << 3 == 0 {
                break; // FR.BUSY: still shifting a byte out
            }
        }
        let control = register(0x30).read_volatile();
        register(0x30).write_volatile(control & !1);
        register(0x24).write_volatile(new_ibrd);
        register(0x28).write_volatile(new_fbrd);
        register(0x2c).write_volatile(register(0x2c).read_volatile());
        register(0x30).write_volatile(control);
        while fifo_read().is_some() {}
        TAIL.store(HEAD.load(Ordering::Relaxed), Ordering::Relaxed);
    }
    true
}

/// Waits until the transmitter has sent everything (bounded).
pub fn drain() {
    for _ in 0..1_000_000 {
        if unsafe { register(0x18).read_volatile() } & 1 << 3 == 0 {
            break;
        }
    }
}

pub struct Serial;

impl core::fmt::Write for Serial {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            if byte == b'\n' {
                put(b'\r');
            }
            put(byte);
        }
        Ok(())
    }
}

/// Received bytes the interrupt handler has taken out of the 16-byte FIFO (single producer, single consumer).
const RING_SIZE: usize = 4096;
static mut RING: [u8; RING_SIZE] = [0; RING_SIZE];
static HEAD: AtomicUsize = AtomicUsize::new(0);
static TAIL: AtomicUsize = AtomicUsize::new(0);
static INTERRUPT: AtomicBool = AtomicBool::new(false);
static INTERRUPTS: AtomicU64 = AtomicU64::new(0);
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// The Pi 4's UART0 is SPI 121, QEMU `virt`'s PL011 SPI 1.
const PI4_INTID: u32 = 32 + 121;
const QEMU_INTID: u32 = 32 + 1;
const IMSC: u64 = 0x38;
const ICR: u64 = 0x44;
const IFLS: u64 = 0x34;
/// Interrupt mask bits: receive (4) and receive timeout (6, the FIFO holds bytes that stopped coming).
const RX_INTERRUPTS: u32 = 1 << 4 | 1 << 6;

/// Receives through the interrupt: the FIFO is emptied into a 4 KiB ring as bytes arrive, so a busy foreground (a flash
/// write, a screen redraw) no longer overruns the 16-byte FIFO, and `wfi` wakes on a typed key. Needs the GIC.
pub fn enable_interrupt(pi4: bool) {
    unsafe {
        register(IFLS).write_volatile(0); // receive trigger at 1/8 full (2 bytes)
        register(ICR).write_volatile(0x7ff);
    }
    crate::gic::register(if pi4 { PI4_INTID } else { QEMU_INTID }, true, interrupt);
    INTERRUPT.store(true, Ordering::Release);
    unsafe { register(IMSC).write_volatile(register(IMSC).read_volatile() | RX_INTERRUPTS) };
}

fn interrupt(_intid: u32) {
    INTERRUPTS.fetch_add(1, Ordering::Relaxed);
    while let Some(byte) = fifo_read() {
        let head = HEAD.load(Ordering::Relaxed);
        let next = (head + 1) % RING_SIZE;
        if next == TAIL.load(Ordering::Acquire) {
            DROPPED.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        unsafe { (*core::ptr::addr_of_mut!(RING))[head] = byte };
        HEAD.store(next, Ordering::Release);
    }
    unsafe { register(ICR).write_volatile(0x7ff) };
}

/// (interrupts taken, bytes dropped because the ring was full).
pub fn interrupt_counts() -> (u64, u64) {
    (INTERRUPTS.load(Ordering::Relaxed), DROPPED.load(Ordering::Relaxed))
}

fn fifo_read() -> Option<u8> {
    if unsafe { register(0x18).read_volatile() } & 1 << 4 != 0 {
        return None; // FR bit 4 RXFE: receive FIFO empty
    }
    Some(unsafe { register(0).read_volatile() } as u8)
}

/// The next received byte, if one is waiting (the ring once the interrupt is on, else the FIFO itself).
pub fn read_byte() -> Option<u8> {
    if !INTERRUPT.load(Ordering::Acquire) {
        return fifo_read();
    }
    let tail = TAIL.load(Ordering::Relaxed);
    if tail == HEAD.load(Ordering::Acquire) {
        // A byte below the FIFO trigger level raises no interrupt (QEMU has no receive timeout); with interrupts off the
        // handler cannot slip newer bytes in ahead of it.
        let was = crate::cpu::interrupts_save_and_disable();
        let byte = if tail == HEAD.load(Ordering::Acquire) { fifo_read() } else { None };
        crate::cpu::interrupts_restore(was);
        if byte.is_some() || tail == HEAD.load(Ordering::Acquire) {
            return byte;
        }
    }
    let byte = unsafe { (*core::ptr::addr_of!(RING))[tail] };
    TAIL.store((tail + 1) % RING_SIZE, Ordering::Release);
    Some(byte)
}
