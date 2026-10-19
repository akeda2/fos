//! Chip driver for the Texas Instruments TUSB3410, as found in the Moxa UPort 1110 (110a:1110). The protocol follows
//! Linux's `ti_usb_3410_5052.c`.
//!
//! The chip powers up in a boot state (one configuration, a bulk OUT endpoint only) and waits for its firmware. The
//! firmware is not in the kernel: it is the file `moxa-1110.fw` on a FOSFS volume (Moxa's, redistributable under
//! linux-firmware's LICENCE.moxa; copy it with `foscopy.py`). After the download the device must be reset; it then comes
//! back as a device with two configurations, of which the second has the bulk IN and OUT endpoints (and an interrupt
//! endpoint for modem status, which is not used: the port reports the DTR/RTS it set, and CTS/DSR read as low).
//!
//! Vendor requests (bmRequestType 0x40) go to the UART module (index 3); register writes go through WRITE_DATA to the RAM
//! module (index 5) with a small header naming an XDATA address, a mask and a value.

use crate::usb_serial::{Attached, Chip, Pipe};
use fosfor_abi::tape::{SERIAL_LINE_DTR, SERIAL_LINE_RTS};

pub struct Ti;

const VENDOR_MOXA: u16 = 0x110a;
const PRODUCT_1110: u16 = 0x1110;
const FIRMWARE: &[u8] = b"moxa-1110.fw";
/// The chip's boot ROM takes at most this much.
const FIRMWARE_MAX: usize = 16 * 1024;

const SET_CONFIG: u8 = 0x05;
const OPEN_PORT: u8 = 0x06;
const CLOSE_PORT: u8 = 0x07;
const START_PORT: u8 = 0x08;
const PURGE_PORT: u8 = 0x0b;
const WRITE_DATA: u8 = 0x80;
const UART1_PORT: u16 = 3;
const RAM_PORT: u16 = 5;
const UART1_BASE: u32 = 0xffa0;
const UART_LCR: u32 = 2;
const UART_MCR: u32 = 4;
const PURGE_INPUT: u16 = 0x80;
const PURGE_OUTPUT: u16 = 0x00;
/// Continuous pipe mode | timeout enabled | a 2 x 10 ms timeout: an idle bulk IN ends with a short packet instead of NAKing.
const OPEN_SETTINGS: u16 = 0x01 | 0x80 | (2 << 2);

static mut IMAGE: [u8; FIRMWARE_MAX + 8] = [0; FIRMWARE_MAX + 8];

impl Chip for Ti {
    fn name(&self) -> &'static str {
        "Moxa UPort 1110 (TI TUSB3410)"
    }

    fn claims(&self, vendor: u16, product: u16) -> bool {
        vendor == VENDOR_MOXA && product == PRODUCT_1110
    }

    fn configuration_index(&self, configurations: u8) -> u8 {
        if configurations > 1 { 1 } else { 0 }
    }

    fn attach(&self, pipe: &mut Pipe, _vendor: u16, _product: u16) -> Attached {
        if pipe.in_ep != 0 {
            return Attached::Ready; // the firmware is running
        }
        match download(pipe) {
            Ok(()) => Attached::Reprobe,
            Err(why) => Attached::Failed(why),
        }
    }

    fn open(&self, pipe: &mut Pipe, baud: u32, format: u32) -> bool {
        let mcr = (SERIAL_LINE_DTR | SERIAL_LINE_RTS) as u8;
        if !configure(pipe, baud, format) || !write_register(pipe, UART_MCR, 0x30 | 0x04, mcr) {
            return false;
        }
        // Open, start, purge, reset the data toggles, and open again (Linux does the same dance; SET_CONFIG raises RTS and DTR).
        let ok = pipe.vendor_out(OPEN_PORT, OPEN_SETTINGS, UART1_PORT, &[])
            && pipe.vendor_out(START_PORT, 0, UART1_PORT, &[])
            && pipe.vendor_out(PURGE_PORT, PURGE_INPUT, UART1_PORT, &[])
            && pipe.vendor_out(PURGE_PORT, PURGE_OUTPUT, UART1_PORT, &[]);
        if !ok {
            return false;
        }
        pipe.clear_halts();
        configure(pipe, baud, format)
            && write_register(pipe, UART_MCR, 0x30 | 0x04, mcr)
            && pipe.vendor_out(OPEN_PORT, OPEN_SETTINGS, UART1_PORT, &[])
            && pipe.vendor_out(START_PORT, 0, UART1_PORT, &[])
    }

    fn close(&self, pipe: &mut Pipe) {
        let _ = pipe.vendor_out(CLOSE_PORT, 0, UART1_PORT, &[]);
        let _ = write_register(pipe, UART_MCR, 0x30 | 0x04, 0);
    }

    fn set_lines(&self, pipe: &mut Pipe, lines: u32) {
        let mcr = (lines & (SERIAL_LINE_DTR | SERIAL_LINE_RTS)) as u8; // the chip's MCR bits are the same as ours
        let _ = write_register(pipe, UART_MCR, 0x30 | 0x04, mcr);
    }

    fn set_break(&self, pipe: &mut Pipe, on: bool) {
        let _ = write_register(pipe, UART_LCR, 0x40, if on { 0x40 } else { 0 });
    }
}

/// SET_CONFIG: baud divisor, flags, data bits, parity, stop bits, XON/XOFF, UART mode (RS-232).
fn configure(pipe: &mut Pipe, baud: u32, format: u32) -> bool {
    let divisor = ((923_077 + baud / 2) / baud) as u16;
    let parity_on = format & 0x08 != 0;
    let flags: u16 = 0x2000 | 0x4000 | if parity_on { 0x0008 } else { 0 };
    let parity = if !parity_on { 0 } else if format & 0x10 != 0 { 2 } else { 1 };
    let stop = if format & 0x04 != 0 { 2 } else { 0 };
    let config = [(divisor >> 8) as u8, divisor as u8, (flags >> 8) as u8, flags as u8, (format & 3) as u8, parity, stop, 0, 0, 0];
    pipe.vendor_out(SET_CONFIG, 0, UART1_PORT, &config)
}

/// Writes `value` under `mask` into one of the UART's registers (an XDATA byte write through WRITE_DATA).
fn write_register(pipe: &mut Pipe, register: u32, mask: u8, value: u8) -> bool {
    let address = UART1_BASE + register;
    let data = [0x30, 0x01, 0x01, (address >> 24) as u8, (address >> 16) as u8, (address >> 8) as u8, address as u8, mask, value];
    // The address is a be16 high half then a be16 low half: bytes 3-4 are the high half, 5-6 the low half.
    pipe.vendor_out(WRITE_DATA, 0, RAM_PORT, &data)
}

/// Sends the firmware to the boot ROM: the file's first three bytes are overwritten with the length of the rest
/// (little-endian) and the sum of the rest, which is what the ROM checks.
fn download(pipe: &mut Pipe) -> Result<(), &'static str> {
    let image = unsafe { &mut *core::ptr::addr_of_mut!(IMAGE) };
    let Some(volume) = crate::storage::volume_with_file(FIRMWARE) else {
        return Err("needs the firmware file moxa-1110.fw on a FOSFS volume (foscopy.py FILE :disk1/moxa-1110.fw)");
    };
    let size = match volume.read(FIRMWARE, &mut image[..FIRMWARE_MAX]) {
        Ok(Some(size)) if size > 3 => size,
        _ => return Err("moxa-1110.fw could not be read (it is at most 16 KiB)"),
    };
    let length = (size - 3) as u16;
    let checksum = image[3..size].iter().fold(0u8, |sum, &byte| sum.wrapping_add(byte));
    image[0] = length as u8;
    image[1] = (length >> 8) as u8;
    image[2] = checksum;
    crate::println!("USB serial: downloading moxa-1110.fw ({size} bytes) to the adapter.");
    let mut sent = 0;
    while sent < size {
        let chunk = (size - sent).min(64);
        match pipe.bulk_out(&image[sent..sent + chunk]) {
            Ok(count) if count == chunk => sent += chunk,
            _ => return Err("the firmware download failed"),
        }
    }
    crate::timer::wait_ticks(10); // let the firmware start
    Ok(())
}
