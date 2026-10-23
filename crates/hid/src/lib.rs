//! HID report descriptor parser for pointer devices.
//!
//! A report descriptor is a stream of short items (prefix byte: size in bits 0-1, type in 2-3, tag in 4-7;
//! size code 3 means 4 bytes). Global items (usage page, logical min/max, report size/count, report id)
//! persist, local items (usage, usage min/max) apply to the next main item only. Each Input item lays out
//! `report_count` fields of `report_size` bits; we record where the ones a pointer needs are.
//!
//! Only what a touch panel, mouse or tablet needs is kept: X, Y, tip switch (or button 1), contact id and
//! contact count. Up to [`MAX_CONTACTS`] contacts are tracked; a multi-touch panel repeats the finger
//! collection once per contact in the descriptor, and each repetition is one [`Contact`] here.

#![no_std]

pub const MAX_CONTACTS: usize = 10;

/// Where a field sits in a report: bit offset from the start of the report data (after any report id byte).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Field {
    pub offset: u16,
    pub size: u8,
    pub min: i32,
    pub max: i32,
    pub signed: bool,
    /// Relative fields (a mouse's X/Y) report movement, not a position.
    pub relative: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Contact {
    pub x: Option<Field>,
    pub y: Option<Field>,
    /// Tip switch (touch) or button 1 (mouse, digitizer pen).
    pub tip: Option<Field>,
    pub id: Option<Field>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Touchscreen,
    Touchpad,
    Digitizer,
    Mouse,
}

#[derive(Clone, Copy, Debug)]
pub struct Pointer {
    pub kind: Kind,
    /// Report id the input report starts with, 0 when the device uses none.
    pub report_id: u8,
    /// Size of the report in bytes, including the id byte.
    pub report_bytes: u16,
    pub contacts: [Contact; MAX_CONTACTS],
    pub contact_count: usize,
    /// "Contact count" field (how many contacts are valid in this report), when present.
    pub count_field: Option<Field>,
}

const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const PAGE_DIGITIZER: u16 = 0x0d;

const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const USAGE_MOUSE: u16 = 0x02;

const DIG_DIGITIZER: u16 = 0x01;
const DIG_PEN: u16 = 0x02;
const DIG_TOUCHSCREEN: u16 = 0x04;
const DIG_TOUCHPAD: u16 = 0x05;
const DIG_TIP_SWITCH: u16 = 0x42;
const DIG_CONTACT_ID: u16 = 0x51;
const DIG_CONTACT_COUNT: u16 = 0x54;

#[derive(Clone, Copy, Default)]
struct Globals {
    page: u16,
    min: i32,
    max: i32,
    size: u32,
    count: u32,
    id: u8,
}

#[derive(Default)]
struct Local {
    usages: [u16; 16],
    count: usize,
    min: u16,
    max: u16,
}

/// Parses `descriptor` and returns the best pointer-like input report, or `None` when the device is not a
/// pointer (a keyboard, a consumer-control interface).
///
/// When a device has several report ids (a touch panel often has one for touch and one for mouse
/// emulation), the one with the most contacts wins; ties go to the first.
pub fn parse_pointer(descriptor: &[u8]) -> Option<Pointer> {
    const IDS: usize = 8;
    let mut reports: [Option<Pointer>; IDS] = [None; IDS];
    let mut offsets = [0u16; IDS];
    let mut ids = [0u8; IDS];
    let mut used = 0usize;
    let mut global = Globals::default();
    let mut stack = [Globals::default(); 4];
    let mut depth = 0;
    let mut local = Local::default();
    let mut application = (0u16, 0u16);
    let mut nesting = 0u32;
    // A finger collection inside the application starts a contact; with those, a repeated field inside one is ignored
    // (an ELAN panel declares X and Y twice per finger).
    let mut grouped = false;
    let mut fresh = false;

    let mut at = 0;
    while at < descriptor.len() {
        let prefix = descriptor[at];
        if prefix == 0xfe {
            // Long item: skip it (data size in the next byte).
            at += 3 + usize::from(*descriptor.get(at + 1)?);
            continue;
        }
        let size = match prefix & 3 {
            3 => 4,
            n => usize::from(n),
        };
        let kind = (prefix >> 2) & 3;
        let tag = prefix >> 4;
        let Some(data) = descriptor.get(at + 1..at + 1 + size) else { break };
        let mut raw = 0u32;
        for (i, byte) in data.iter().enumerate() {
            raw |= u32::from(*byte) << (8 * i);
        }
        let signed_value = match size {
            1 => i32::from(raw as u8 as i8),
            2 => i32::from(raw as u16 as i16),
            _ => raw as i32,
        };
        at += 1 + size;

        match (kind, tag) {
            (1, 0) => global.page = raw as u16,
            (1, 1) => global.min = signed_value,
            // A logical maximum is signed only when the minimum is negative (0xff as max is 255, not -1).
            (1, 2) => global.max = if global.min < 0 { signed_value } else { raw as i32 },
            (1, 7) => global.size = raw,
            (1, 8) => global.id = raw as u8,
            (1, 9) => global.count = raw,
            (1, 10) => {
                if depth < stack.len() {
                    stack[depth] = global;
                    depth += 1;
                }
            }
            (1, 11) => {
                if depth > 0 {
                    depth -= 1;
                    global = stack[depth];
                }
            }
            (2, 0) => {
                if local.count < local.usages.len() {
                    local.usages[local.count] = raw as u16;
                    local.count += 1;
                }
            }
            (2, 1) => local.min = raw as u16,
            (2, 2) => local.max = raw as u16,
            (0, 10) => {
                if nesting == 0 {
                    application = (global.page, if local.count > 0 { local.usages[0] } else { 0 });
                }
                if nesting >= 1 {
                    grouped = true;
                    fresh = true;
                }
                nesting += 1;
                local = Local::default();
            }
            (0, 12) => {
                nesting = nesting.saturating_sub(1);
                local = Local::default();
            }
            (0, 8) => {
                let constant = raw & 1 != 0;
                let relative = raw & 4 != 0;
                let slot = match ids[..used].iter().position(|id| *id == global.id) {
                    Some(slot) => Some(slot),
                    None if used < IDS => {
                        ids[used] = global.id;
                        used += 1;
                        Some(used - 1)
                    }
                    None => None,
                };
                if let Some(slot) = slot {
                    let mut position = offsets[slot];
                    let report = reports[slot].get_or_insert(Pointer {
                        kind: kind_of(application),
                        report_id: global.id,
                        report_bytes: 0,
                        contacts: [Contact::default(); MAX_CONTACTS],
                        contact_count: 0,
                        count_field: None,
                    });
                    if constant {
                        position = position.saturating_add((global.size * global.count) as u16);
                    } else {
                        for index in 0..global.count {
                            let usage = usage_at(&local, index);
                            let field = Field { offset: position, size: global.size as u8, min: global.min, max: global.max, signed: global.min < 0, relative };
                            place(report, global.page, usage, field, grouped, &mut fresh);
                            position = position.saturating_add(global.size as u16);
                        }
                    }
                    offsets[slot] = position;
                }
                local = Local::default();
            }
            (0, 9) | (0, 11) => local = Local::default(),
            _ => {}
        }
    }

    let mut best: Option<Pointer> = None;
    for slot in 0..used {
        let Some(mut report) = reports[slot] else { continue };
        report.report_bytes = offsets[slot].div_ceil(8) + u16::from(report.report_id != 0);
        if !report.contacts[..report.contact_count].iter().any(|c| c.x.is_some() && c.y.is_some()) {
            continue;
        }
        if best.as_ref().is_none_or(|old| report.contact_count > old.contact_count) {
            best = Some(report);
        }
    }
    best
}

fn kind_of(application: (u16, u16)) -> Kind {
    match application {
        (PAGE_DIGITIZER, DIG_TOUCHSCREEN) => Kind::Touchscreen,
        (PAGE_DIGITIZER, DIG_TOUCHPAD) => Kind::Touchpad,
        (PAGE_DIGITIZER, DIG_DIGITIZER) | (PAGE_DIGITIZER, DIG_PEN) => Kind::Digitizer,
        (PAGE_GENERIC_DESKTOP, USAGE_MOUSE) => Kind::Mouse,
        _ => Kind::Digitizer,
    }
}

fn usage_at(local: &Local, index: u32) -> u16 {
    if local.count > 0 {
        // A short usage list applies its last entry to the remaining fields.
        local.usages[(index as usize).min(local.count - 1)]
    } else if local.min != 0 && local.max >= local.min {
        local.min.saturating_add(index as u16).min(local.max)
    } else {
        0
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    X,
    Y,
    Tip,
    Id,
}

/// Files one field into the pointer's contact list. A new contact starts when a field that already exists in
/// the current contact appears again (the finger collection repeats).
fn place(report: &mut Pointer, page: u16, usage: u16, field: Field, grouped: bool, fresh: &mut bool) {
    let (slot, from_button) = match (page, usage) {
        (PAGE_GENERIC_DESKTOP, USAGE_X) => (Slot::X, false),
        (PAGE_GENERIC_DESKTOP, USAGE_Y) => (Slot::Y, false),
        (PAGE_DIGITIZER, DIG_TIP_SWITCH) => (Slot::Tip, false),
        (PAGE_DIGITIZER, DIG_CONTACT_ID) => (Slot::Id, false),
        (PAGE_BUTTON, 1) => (Slot::Tip, true),
        (PAGE_DIGITIZER, DIG_CONTACT_COUNT) => {
            report.count_field = Some(field);
            return;
        }
        _ => return,
    };
    if report.contact_count == 0 {
        report.contact_count = 1;
    }
    let mut index = report.contact_count - 1;
    if *fresh && !from_button {
        *fresh = false;
        let c = &report.contacts[index];
        if (c.x.is_some() || c.y.is_some() || c.id.is_some()) && report.contact_count < MAX_CONTACTS {
            report.contact_count += 1;
            index += 1;
        }
    } else if grouped && get(&report.contacts[index], slot).is_some() {
        return;
    }
    if get(&report.contacts[index], slot).is_some() {
        // Button 1 only stands in for a tip switch; it never starts a new contact.
        if from_button || report.contact_count >= MAX_CONTACTS {
            return;
        }
        report.contact_count += 1;
        index += 1;
    }
    let contact = &mut report.contacts[index];
    match slot {
        Slot::X => contact.x = Some(field),
        Slot::Y => contact.y = Some(field),
        Slot::Tip => contact.tip = Some(field),
        Slot::Id => contact.id = Some(field),
    }
}

fn get(contact: &Contact, slot: Slot) -> Option<Field> {
    match slot {
        Slot::X => contact.x,
        Slot::Y => contact.y,
        Slot::Tip => contact.tip,
        Slot::Id => contact.id,
    }
}

/// Reads `field` from `report` (the bytes after the report id), sign-extended when the field is signed.
pub fn extract(report: &[u8], field: &Field) -> Option<i32> {
    let size = u32::from(field.size);
    if size == 0 || size > 32 {
        return None;
    }
    let start = u32::from(field.offset);
    if (start + size).div_ceil(8) as usize > report.len() {
        return None;
    }
    let mut value = 0u64;
    for bit in 0..size {
        let at = start + bit;
        if report[(at / 8) as usize] >> (at % 8) & 1 != 0 {
            value |= 1 << bit;
        }
    }
    if field.signed && size < 32 && value >> (size - 1) & 1 != 0 {
        value |= !0u64 << size;
    }
    Some(value as u32 as i32)
}

/// One decoded contact, in the device's own coordinates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Touch {
    pub id: u8,
    pub x: i32,
    pub y: i32,
    pub down: bool,
}

impl Pointer {
    /// Decodes one input report (id byte included, if the device uses ids) into up to
    /// [`MAX_CONTACTS`] touches; returns how many were written. Reports of other ids give 0.
    pub fn decode(&self, report: &[u8], out: &mut [Touch; MAX_CONTACTS]) -> usize {
        let body = if self.report_id != 0 {
            match report.split_first() {
                Some((id, rest)) if *id == self.report_id => rest,
                _ => return 0,
            }
        } else {
            report
        };
        let valid = match self.count_field.and_then(|field| extract(body, &field)) {
            Some(count) => (count.max(0) as usize).min(self.contact_count),
            None => self.contact_count,
        };
        let mut written = 0;
        for (index, contact) in self.contacts[..self.contact_count].iter().enumerate() {
            let (Some(x), Some(y)) = (contact.x, contact.y) else { continue };
            if self.count_field.is_some() && index >= valid {
                continue;
            }
            let (Some(vx), Some(vy)) = (extract(body, &x), extract(body, &y)) else { continue };
            let down = match contact.tip {
                Some(tip) => extract(body, &tip).unwrap_or(0) != 0,
                None => true,
            };
            let id = contact.id.and_then(|field| extract(body, &field)).unwrap_or(index as i32);
            out[written] = Touch { id: id as u8, x: vx, y: vy, down };
            written += 1;
        }
        written
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A typical single-touch panel: tip switch + 7 padding bits, then X and Y 0..4095 in 16 bits each.
    const SINGLE_TOUCH: &[u8] = &[
        0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, // Digitizer, Touch Screen, Application
        0x09, 0x22, 0xa1, 0x00, // Finger, Physical
        0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, // Tip Switch
        0x75, 0x07, 0x95, 0x01, 0x81, 0x03, // 7 bits padding
        0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xff, 0x0f, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, // X, Y
        0xc0, 0xc0,
    ];

    #[test]
    fn single_touch() {
        let pointer = parse_pointer(SINGLE_TOUCH).unwrap();
        assert_eq!(pointer.kind, Kind::Touchscreen);
        assert_eq!(pointer.contact_count, 1);
        let contact = pointer.contacts[0];
        assert_eq!(contact.tip.unwrap().offset, 0);
        assert_eq!(contact.x.unwrap(), Field { offset: 8, size: 16, min: 0, max: 4095, signed: false, relative: false });
        assert_eq!(contact.y.unwrap().offset, 24);
        assert_eq!(pointer.report_bytes, 5);
        let mut out = [Touch::default(); MAX_CONTACTS];
        assert_eq!(pointer.decode(&[1, 0x34, 0x02, 0x78, 0x01], &mut out), 1);
        assert_eq!(out[0], Touch { id: 0, x: 0x234, y: 0x178, down: true });
        assert_eq!(pointer.decode(&[0, 0x34, 0x02, 0x78, 0x01], &mut out), 1);
        assert!(!out[0].down);
    }

    // Two fingers, report id 1: per finger tip(1) pad(3) id(4) x(11) y(11) pad(2) = 4 bytes, then count(8).
    const MULTI_TOUCH: &[u8] = &[
        0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x85, 0x01, //
        0x09, 0x22, 0xa1, 0x02, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, //
        0x75, 0x03, 0x95, 0x01, 0x81, 0x03, 0x09, 0x51, 0x75, 0x04, 0x95, 0x01, 0x25, 0x0f, 0x81, 0x02, //
        0x05, 0x01, 0x09, 0x30, 0x26, 0xff, 0x07, 0x75, 0x0b, 0x95, 0x01, 0x81, 0x02, //
        0x09, 0x31, 0x26, 0xff, 0x07, 0x81, 0x02, 0x75, 0x02, 0x81, 0x03, 0xc0, //
        0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x09, 0x42, 0x75, 0x01, 0x25, 0x01, 0x81, 0x02, //
        0x75, 0x03, 0x81, 0x03, 0x09, 0x51, 0x75, 0x04, 0x25, 0x0f, 0x81, 0x02, //
        0x05, 0x01, 0x09, 0x30, 0x26, 0xff, 0x07, 0x75, 0x0b, 0x81, 0x02, //
        0x09, 0x31, 0x81, 0x02, 0x75, 0x02, 0x81, 0x03, 0xc0, //
        0x05, 0x0d, 0x09, 0x54, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, 0xc0,
    ];

    // ELAN style: X and Y are declared with count 2 inside each finger collection; the finger collection, not the repeated
    // field, starts a contact. Per finger: tip(1) pad(7) x(16) x2(16) y(16) y2(16) = 9 bytes.
    const ELAN_TOUCH: &[u8] = &[
        0x05, 0x0d, 0x09, 0x04, 0xa1, 0x01, 0x85, 0x01, //
        0x09, 0x22, 0xa1, 0x02, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x03, //
        0x05, 0x01, 0x09, 0x30, 0x26, 0xf0, 0x0c, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x26, 0x50, 0x07, 0x09, 0x31, 0x81, 0x02, 0xc0, //
        0x05, 0x0d, 0x09, 0x22, 0xa1, 0x02, 0x09, 0x42, 0x75, 0x01, 0x95, 0x01, 0x25, 0x01, 0x81, 0x02, 0x75, 0x07, 0x81, 0x03, //
        0x05, 0x01, 0x09, 0x30, 0x26, 0xf0, 0x0c, 0x75, 0x10, 0x95, 0x02, 0x81, 0x02, 0x26, 0x50, 0x07, 0x09, 0x31, 0x81, 0x02, 0xc0, 0xc0,
    ];

    #[test]
    fn repeated_fields_stay_in_one_contact() {
        let pointer = parse_pointer(ELAN_TOUCH).unwrap();
        assert_eq!(pointer.contact_count, 2);
        assert_eq!(pointer.contacts[0].x.unwrap().offset, 8);
        assert_eq!(pointer.contacts[0].y.unwrap().offset, 40);
        assert_eq!(pointer.contacts[0].y.unwrap().max, 1872);
        assert_eq!(pointer.contacts[1].tip.unwrap().offset, 72);
        assert_eq!(pointer.contacts[1].y.unwrap().max, 1872);
    }

    #[test]
    fn multi_touch() {
        let pointer = parse_pointer(MULTI_TOUCH).unwrap();
        assert_eq!(pointer.report_id, 1);
        assert_eq!(pointer.contact_count, 2);
        assert!(pointer.count_field.is_some());
        let mut report = [0u8; 10];
        report[0] = 1;
        // Finger 0: tip=1 id=3, x=100, y=200. Bits: tip@0, id@4..8, x@8..19, y@19..30.
        report[1] = 1 | 3 << 4;
        report[2] = 100;
        report[3] = (200u32 & 0x1f) as u8 * 8; // x high bits 0, y low 5 bits at bit 19..24
        report[4] = (200u32 >> 5) as u8;
        // Finger 1 starts at byte 4+1=5 of the body: tip=1 id=4, x=0x7ff, y=5.
        report[5] = 1 | 4 << 4;
        report[6] = 0xff;
        report[7] = 0x07 | (5u32 & 0x1f) as u8 * 8;
        report[8] = 0;
        report[9] = 2; // contact count
        let mut out = [Touch::default(); MAX_CONTACTS];
        assert_eq!(pointer.decode(&report, &mut out), 2);
        assert_eq!(out[0], Touch { id: 3, x: 100, y: 200, down: true });
        assert_eq!(out[1], Touch { id: 4, x: 0x7ff, y: 5, down: true });
        report[9] = 1;
        assert_eq!(pointer.decode(&report, &mut out), 1);
    }

    // Boot mouse: three buttons, relative X/Y.
    const MOUSE: &[u8] = &[
        0x05, 0x01, 0x09, 0x02, 0xa1, 0x01, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02,
        0x95, 0x01, 0x75, 0x05, 0x81, 0x03, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95, 0x02, 0x81, 0x06, 0xc0, 0xc0,
    ];

    #[test]
    fn mouse_is_relative() {
        let pointer = parse_pointer(MOUSE).unwrap();
        assert_eq!(pointer.kind, Kind::Mouse);
        let contact = pointer.contacts[0];
        assert!(contact.x.unwrap().relative && contact.x.unwrap().signed);
        let mut out = [Touch::default(); MAX_CONTACTS];
        assert_eq!(pointer.decode(&[1, 0xfe, 5], &mut out), 1);
        assert_eq!((out[0].x, out[0].y, out[0].down), (-2, 5, true));
    }

    #[test]
    fn keyboard_is_not_a_pointer() {
        let keyboard = [0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0xc0];
        assert!(parse_pointer(&keyboard).is_none());
    }

    #[test]
    fn truncated_descriptor_does_not_panic() {
        for cut in 0..MULTI_TOUCH.len() {
            let _ = parse_pointer(&MULTI_TOUCH[..cut]);
        }
    }
}
