//! Chip driver for the Prolific PL2303 family (067b:2303), the common cheap USB to RS-232 adapter. The protocol follows
//! Linux's `pl2303.c`.
//!
//! No firmware: the chip enumerates with one configuration holding an interrupt IN endpoint (modem status, not used:
//! CTS/DSR read as low, the port reports the DTR/RTS it set) and bulk IN and OUT endpoints that carry raw data.
//! Line settings and the modem lines are CDC-style class requests (SET_LINE_CODING 0x20, SET_CONTROL_LINE_STATE 0x22,
//! SEND_BREAK 0x23); the rest are vendor requests (request 1, the register in `value`). After opening, the data
//! pipes are reset with registers 8 and 9, and flow control is switched off (register 0), which the chip would
//! otherwise inherit from whatever the last user left. Only the HX/HXD/TA generation is assumed (full-speed,
//! 64-byte control endpoint): the vendor init below is what Linux ran for it before it learned to tell chips apart.

use crate::usb_serial::{Attached, Chip, Pipe};
use fosfor_abi::tape::{SERIAL_LINE_DTR, SERIAL_LINE_RTS};

pub struct Pl2303;

const VENDOR_PROLIFIC: u16 = 0x067b;
const PRODUCT_PL2303: u16 = 0x2303;

const VENDOR_OUT: u8 = 0x40;
const VENDOR_IN: u8 = 0xc0;
const CLASS_OUT: u8 = 0x21;
const SET_LINE_CODING: u8 = 0x20;
const SET_CONTROL_LINE_STATE: u8 = 0x22;
const SEND_BREAK: u8 = 0x23;

fn vendor_read(pipe: &mut Pipe, value: u16) -> bool {
    let mut byte = [0u8; 1];
    pipe.control(VENDOR_IN, 1, value, 0, &mut byte)
}

fn vendor_write(pipe: &mut Pipe, value: u16, index: u16) -> bool {
    pipe.control(VENDOR_OUT, 1, value, index, &mut [])
}

impl Chip for Pl2303 {
    fn name(&self) -> &'static str {
        "Prolific PL2303"
    }

    fn claims(&self, vendor: u16, product: u16) -> bool {
        vendor == VENDOR_PROLIFIC && product == PRODUCT_PL2303
    }

    fn configuration_index(&self, _configurations: u8) -> u8 {
        0
    }

    fn attach(&self, pipe: &mut Pipe, _vendor: u16, _product: u16) -> Attached {
        if pipe.in_ep == 0 || pipe.out_ep == 0 {
            return Attached::Failed("has no bulk endpoints");
        }
        // The vendor handshake the chip expects once after it is plugged in (the reads are part of it, their data is not used).
        let ok = vendor_read(pipe, 0x8484)
            && vendor_write(pipe, 0x0404, 0)
            && vendor_read(pipe, 0x8484)
            && vendor_read(pipe, 0x8383)
            && vendor_read(pipe, 0x8484)
            && vendor_write(pipe, 0x0404, 1)
            && vendor_read(pipe, 0x8484)
            && vendor_read(pipe, 0x8383)
            && vendor_write(pipe, 0, 1)
            && vendor_write(pipe, 1, 0)
            && vendor_write(pipe, 2, 0x44);
        if ok { Attached::Ready } else { Attached::Failed("did not answer its vendor requests (a clone chip, or not an HX/TA?)") }
    }

    fn open(&self, pipe: &mut Pipe, baud: u32, format: u32) -> bool {
        let parity_on = format & 0x08 != 0;
        let parity = if !parity_on { 0 } else if format & 0x10 != 0 { 2 } else { 1 };
        let stop = if format & 0x04 != 0 { 2 } else { 0 };
        let mut coding = [0u8; 7];
        coding[..4].copy_from_slice(&baud.to_le_bytes());
        coding[4] = stop;
        coding[5] = parity;
        coding[6] = 5 + (format & 3) as u8;
        pipe.clear_halts();
        pipe.control(CLASS_OUT, SET_LINE_CODING, 0, 0, &mut coding)
            && vendor_write(pipe, 0, 0) // no hardware flow control
            && vendor_write(pipe, 8, 0)
            && vendor_write(pipe, 9, 0) // reset the upstream data pipes
            && pipe.control(CLASS_OUT, SET_CONTROL_LINE_STATE, ((SERIAL_LINE_DTR | SERIAL_LINE_RTS) >> 4) as u16, 0, &mut [])
    }

    fn close(&self, pipe: &mut Pipe) {
        let _ = pipe.control(CLASS_OUT, SET_CONTROL_LINE_STATE, 0, 0, &mut []);
    }

    fn set_lines(&self, pipe: &mut Pipe, lines: u32) {
        // DTR is bit 0 and RTS bit 1 of the request value; ours are bits 4 and 5.
        let _ = pipe.control(CLASS_OUT, SET_CONTROL_LINE_STATE, ((lines & (SERIAL_LINE_DTR | SERIAL_LINE_RTS)) >> 4) as u16, 0, &mut []);
    }

    fn set_break(&self, pipe: &mut Pipe, on: bool) {
        let _ = pipe.control(CLASS_OUT, SEND_BREAK, if on { 0xffff } else { 0 }, 0, &mut []);
    }
}
