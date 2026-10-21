//! First look at a USB pointer device (touch panel, mouse, tablet) nothing claims yet.
//!
//! For every HID interface in the configuration this reads the report descriptor (GET_DESCRIPTOR(report), sent to the
//! interface: request type 0x81, value 0x2200), prints it as hex and prints what `hid::parse_pointer` makes of it: the
//! report id, and for each contact where tip, id, X and Y sit in the report and what range they cover. The device is
//! not configured and the slot is released by the caller. Shared by both kernels (xHCI only).

use crate::xhci::{self, Controller};
use core::fmt::Write;

pub fn describe(controller: &mut Controller, device: &xhci::Device, configuration: &[u8], vendor: u16, product: u16) {
    let mut at = 0;
    let mut interface = None;
    while at + 2 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            return;
        }
        match configuration[at + 1] {
            4 if length >= 9 => interface = (configuration[at + 5] == 3).then_some(configuration[at + 2]),
            // HID descriptor: bLength, 0x21, bcdHID(2), country, numDescriptors, then (type, length(2)) pairs.
            0x21 if length >= 9 && configuration[at + 5] >= 1 && configuration[at + 6] == 0x22 => {
                if let Some(number) = interface {
                    let mut buffer = [0u8; 1024];
                    let wanted = usize::from(u16::from_le_bytes([configuration[at + 7], configuration[at + 8]])).min(buffer.len());
                    match controller.control_transfer(device, 0x81, 0x06, 0x2200, u16::from(number), &mut buffer[..wanted]) {
                        Ok(got) => report(vendor, product, number, &buffer[..got.min(wanted)]),
                        Err(error) => crate::println!("USB HID {vendor:04x}:{product:04x} interface {number}: report descriptor not readable ({error:?})."),
                    }
                }
            }
            _ => {}
        }
        at += length;
    }
}

fn report(vendor: u16, product: u16, interface: u8, descriptor: &[u8]) {
    crate::println!("USB HID {vendor:04x}:{product:04x} interface {interface}: report descriptor, {} bytes:", descriptor.len());
    for row in descriptor.chunks(24) {
        let mut line = crate::netdump::Line::new();
        for byte in row {
            let _ = write!(line, " {byte:02x}");
        }
        crate::println!(" {}", line.as_str());
    }
    let Some(pointer) = hid::parse_pointer(descriptor) else {
        crate::println!("  not a pointer device.");
        return;
    };
    crate::println!(
        "  {:?}: report id {}, {} bytes, {} contact(s){}.",
        pointer.kind,
        pointer.report_id,
        pointer.report_bytes,
        pointer.contact_count,
        if pointer.count_field.is_some() { ", contact count field" } else { "" }
    );
    for (index, contact) in pointer.contacts[..pointer.contact_count].iter().enumerate() {
        let mut line = crate::netdump::Line::new();
        for (name, field) in [("tip", contact.tip), ("id", contact.id), ("x", contact.x), ("y", contact.y)] {
            if let Some(f) = field {
                let _ = write!(line, " {name}@{}:{} [{}..{}]{}", f.offset, f.size, f.min, f.max, if f.relative { " rel" } else { "" });
            }
        }
        crate::println!("  contact {index}:{}", line.as_str());
    }
}
