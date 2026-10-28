//! USB touch panels (HID digitizers) and mice on xHCI, shared by both kernels.
//!
//! For every HID interface in the configuration this reads the report descriptor (GET_DESCRIPTOR(report), sent to the
//! interface: request type 0x81, value 0x2200) and lets `hid::parse_pointer` say where tip, id, X and Y sit in a
//! report. A touch screen or digitizer is claimed: its interrupt-IN endpoint is configured and one report-sized
//! transfer is kept queued; each completed report is decoded (`hid::Pointer::decode`) in `background_event` (called
//! wherever the xHCI event ring is drained, like the keyboards') into per-finger events (down, moved, up) in a ring
//! that `take` empties. Coordinates are the panel's own (`range` gives the extent). Anything else (a mouse, a
//! touchpad, an interface that is not a pointer) is only described: the descriptor as hex and what the parser made
//! of it, and the slot is released by the caller.
//!
//! Mice: a boot-protocol mouse interface with relative X/Y (a USB mouse, or the mouse half of a laptop's USB touchpad) is
//! claimed too. Every mouse (these and the PS/2 one in `keyboard.rs`) moves one shared cursor kept in the tape screen's
//! pixels, and shows up to tapes as a panel (`MOUSE_PANEL`, extent = the screen) with finger id `MOUSE_ID`: pressed or
//! dragged with the left button is `down`, a move with it up is `down` false at the new position (hover, so a tape can draw
//! the cursor), the release is `down` false. No new tape call: a click is a one-finger tap to every touch tape.
//!
//! Written from the HID and Windows 8 multitouch descriptors; the first panel it ran against is an ELAN 04f3:249f
//! (10 contacts, 3312 x 1872) on a Raspberry Pi 4's USB hub.

use crate::memory;
use crate::xhci::{self, Controller, Device, Trb};
use core::fmt::Write;

const MAX_PANELS: usize = 4;
const EVENTS: usize = 128;

#[derive(Clone, Copy)]
struct Panel {
    bus: u8,
    device_number: u8,
    function: u8,
    device: Device,
    vendor: u16,
    product: u16,
    dci: u32,
    pointer: hid::Pointer,
    /// A page: the report lands here.
    buffer: u64,
    length: u32,
    /// The fingers that are down, by contact id, to turn reports into changes.
    down: [Option<hid::Touch>; hid::MAX_CONTACTS],
    reports: u64,
    events: u64,
    /// Reports that ended in an error, and the last completion code (a stalled or failing panel).
    errors: u64,
    last_code: u8,
}

/// A finger went down, moved or lifted (`down` false), at the panel's own coordinates.
#[derive(Clone, Copy, Default)]
pub struct TouchEvent {
    pub panel: u8,
    pub id: u8,
    pub x: i32,
    pub y: i32,
    pub down: bool,
}

/// The panel number the shared mouse cursor reports as (after the USB panels' and the DSI display's numbers).
pub const MOUSE_PANEL: usize = MAX_PANELS + 1;
/// The finger id of the mouse's left button and cursor.
pub const MOUSE_ID: u8 = 100;

/// The cursor in the tape screen's pixels, whether the left button is down, whether a PS/2 mouse is there, mouse packets seen.
static mut CURSOR: (i32, i32) = (-1, -1);
static mut BUTTON: bool = false;
static PS2_MOUSE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static MOUSE_PACKETS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

static mut PANELS: [Option<Panel>; MAX_PANELS] = [None; MAX_PANELS];
static mut QUEUE: [TouchEvent; EVENTS] = [TouchEvent { panel: 0, id: 0, x: 0, y: 0, down: false }; EVENTS];
static mut HEAD: usize = 0;
static mut TAIL: usize = 0;
static mut DROPPED: u64 = 0;

/// Claims a touch screen among the device's HID interfaces and starts it; anything else is described. True when claimed
/// (the slot is then kept), false when the caller should release the slot.
pub fn claim(controller: &mut Controller, device: &xhci::Device, configuration: &[u8], vendor: u16, product: u16) -> bool {
    let mut at = 0;
    let mut interface = None;
    let mut protocol = 0;
    while at + 2 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            return false;
        }
        match configuration[at + 1] {
            4 if length >= 9 => {
                interface = (configuration[at + 5] == 3).then_some(configuration[at + 2]);
                protocol = configuration[at + 7]; // 2: a boot mouse
            }
            // HID descriptor: bLength, 0x21, bcdHID(2), country, numDescriptors, then (type, length(2)) pairs.
            0x21 if length >= 9 && configuration[at + 5] >= 1 && configuration[at + 6] == 0x22 => {
                if let Some(number) = interface {
                    let mut buffer = [0u8; 1024];
                    let wanted = usize::from(u16::from_le_bytes([configuration[at + 7], configuration[at + 8]])).min(buffer.len());
                    match controller.control_transfer(device, 0x81, 0x06, 0x2200, u16::from(number), &mut buffer[..wanted]) {
                        Ok(got) => {
                            let descriptor = &buffer[..got.min(wanted)];
                            // Only a relative boot mouse counts as a mouse: a touch panel's mouse-emulation interface must not
                            // put a second, competing cursor on the screen.
                            let touch = hid::parse_pointer(descriptor).filter(|p| {
                                (matches!(p.kind, hid::Kind::Touchscreen | hid::Kind::Digitizer) && p.contact_count > 0)
                                    || (p.kind == hid::Kind::Mouse && protocol == 2 && p.contacts[0].x.is_some_and(|f| f.relative) && p.contacts[0].tip.is_some())
                            });
                            report(vendor, product, number, descriptor);
                            if touch.is_some_and(|pointer| attach(controller, device, configuration, number, pointer, vendor, product)) {
                                return true;
                            }
                        }
                        Err(error) => crate::println!("USB HID {vendor:04x}:{product:04x} interface {number}: report descriptor not readable ({error:?})."),
                    }
                }
            }
            _ => {}
        }
        at += length;
    }
    false
}

/// The interrupt-IN endpoint of interface `number`: (address, max packet, bInterval).
fn find_endpoint(configuration: &[u8], number: u8) -> Option<(u8, u16, u8)> {
    let mut at = 0;
    let mut inside = false;
    while at + 2 <= configuration.len() {
        let length = usize::from(configuration[at]);
        if length < 2 || at + length > configuration.len() {
            break;
        }
        match configuration[at + 1] {
            4 if length >= 9 => inside = configuration[at + 2] == number && configuration[at + 3] == 0,
            5 if length >= 7 && inside && configuration[at + 2] & 0x80 != 0 && configuration[at + 3] & 3 == 3 => {
                return Some((configuration[at + 2], u16::from_le_bytes([configuration[at + 4], configuration[at + 5]]) & 0x7ff, configuration[at + 6]));
            }
            _ => {}
        }
        at += length;
    }
    None
}

fn attach(controller: &mut Controller, device: &Device, configuration: &[u8], interface: u8, pointer: hid::Pointer, vendor: u16, product: u16) -> bool {
    let panels = unsafe { &mut *core::ptr::addr_of_mut!(PANELS) };
    let Some(entry) = panels.iter_mut().find(|entry| entry.is_none()) else { return false };
    let Some((endpoint, max_packet, interval)) = find_endpoint(configuration, interface) else {
        crate::println!("USB touch {vendor:04x}:{product:04x}: interface {interface} has no interrupt-IN endpoint.");
        return false;
    };
    let Some(buffer) = memory::allocate_contiguous(1, Some(1 << 32)) else { return false };
    if let Err(error) = controller.set_configuration(device, configuration[5]) {
        crate::println!("USB touch {vendor:04x}:{product:04x}: SET_CONFIGURATION failed ({error:?}).");
        memory::free(buffer, 1);
        return false;
    }
    let _ = controller.control_transfer(device, 0x21, 0x0a, 0, u16::from(interface), &mut []); // SET_IDLE(0); may stall
    let dci = match controller.configure_interrupt_in(device, endpoint, max_packet, interval) {
        Ok(dci) => dci,
        Err(error) => {
            crate::println!("USB touch {vendor:04x}:{product:04x}: endpoint {endpoint:#04x} ({max_packet} bytes, interval {interval}) not configured ({error:?}).");
            memory::free(buffer, 1);
            return false;
        }
    };
    // One transfer as long as the whole report (a panel with 10 contacts sends more than one packet), ended early by a short packet.
    let length = u32::from(pointer.report_bytes).clamp(u32::from(max_packet.max(8)), 4096);
    *entry = Some(Panel {
        bus: controller.pci.bus,
        device_number: controller.pci.device,
        function: controller.pci.function,
        device: *device,
        vendor,
        product,
        dci,
        pointer,
        buffer,
        length,
        down: [None; hid::MAX_CONTACTS],
        reports: 0,
        events: 0,
        errors: 0,
        last_code: 0,
    });
    controller.queue_interrupt_in(device.slot, dci, buffer, length);
    let (x, y) = extent(&pointer);
    if pointer.kind == hid::Kind::Mouse {
        crate::println!("USB mouse {vendor:04x}:{product:04x} on port {} ({}): {}-byte reports. Tapes see it as touch panel {}; `touch monitor` shows it.", device.path(), xhci::speed_name(device.speed), pointer.report_bytes, MOUSE_PANEL + 1);
        return true;
    }
    crate::println!("USB touch {vendor:04x}:{product:04x} on port {} ({}): {} contact(s), {x} x {y}, {}-byte reports. `touch monitor` shows it.", device.path(), xhci::speed_name(device.speed), pointer.contact_count, pointer.report_bytes);
    true
}

fn extent(pointer: &hid::Pointer) -> (i32, i32) {
    let contact = &pointer.contacts[0];
    (contact.x.map_or(0, |f| f.max), contact.y.map_or(0, |f| f.max))
}

fn queue_push(event: TouchEvent) {
    unsafe {
        let next = (HEAD + 1) % EVENTS;
        if next == TAIL {
            DROPPED += 1;
            return;
        }
        (*core::ptr::addr_of_mut!(QUEUE))[HEAD] = event;
        HEAD = next;
    }
}

/// Handles a completed report transfer of one of our panels: decodes it, queues what changed and queues the transfer again.
pub fn background_event(controller: &mut Controller, event: &Trb) -> bool {
    let panels = unsafe { &mut *core::ptr::addr_of_mut!(PANELS) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    let Some((index, panel)) = panels.iter_mut().enumerate().find_map(|(index, entry)| {
        let panel = entry.as_mut()?;
        ((panel.bus, panel.device_number, panel.function) == identity && panel.device.slot == event.slot_number() && panel.dci == event.endpoint_index()).then_some((index, panel))
    }) else {
        return false;
    };
    match event.completion() {
        xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT => {
            let got = panel.length.saturating_sub(event.residual()).min(4096) as usize;
            let report = unsafe { core::slice::from_raw_parts(panel.buffer as *const u8, got) };
            let mut touches = [hid::Touch::default(); hid::MAX_CONTACTS];
            let count = panel.pointer.decode(report, &mut touches);
            panel.reports += 1;
            xhci::LAST_TOUCH_US.store(crate::timer::micros(), core::sync::atomic::Ordering::Relaxed);
            if panel.pointer.kind == hid::Kind::Mouse {
                // A mouse's "contact" is its movement since the last report and its button 1.
                if count > 0 {
                    mouse_input(touches[0].x, touches[0].y, touches[0].down);
                }
                controller.queue_interrupt_in(panel.device.slot, panel.dci, panel.buffer, panel.length);
                return true;
            }
            for touch in touches[..count].iter().copied() {
                let known = panel.down.iter().position(|entry| entry.is_some_and(|known| known.id == touch.id));
                let changed = match (known, touch.down) {
                    (Some(at), true) => (panel.down[at] != Some(touch)).then(|| panel.down[at] = Some(touch)).is_some(),
                    (Some(at), false) => {
                        panel.down[at] = None;
                        true
                    }
                    (None, true) => match panel.down.iter().position(Option::is_none) {
                        Some(free) => {
                            panel.down[free] = Some(touch);
                            true
                        }
                        None => false,
                    },
                    (None, false) => false,
                };
                if changed {
                    panel.events += 1;
                    queue_push(TouchEvent { panel: index as u8, id: touch.id, x: touch.x, y: touch.y, down: touch.down });
                }
            }
        }
        code => {
            panel.errors += 1;
            panel.last_code = code;
            controller.reset_stalled_endpoint(panel.device.slot, panel.dci)
        }
    }
    controller.queue_interrupt_in(panel.device.slot, panel.dci, panel.buffer, panel.length);
    true
}

/// Forgets the panels in the slots of `port` (only the one at `route` below it if given); call before the slots go.
pub fn release(controller: &mut Controller, port: u8, route: Option<u32>) {
    let panels = unsafe { &mut *core::ptr::addr_of_mut!(PANELS) };
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    for entry in panels.iter_mut() {
        let Some(panel) = *entry else { continue };
        let there = controller.slot_location(panel.device.slot).is_some_and(|(p, r)| p == port && route.is_none_or(|wanted| wanted == r));
        if (panel.bus, panel.device_number, panel.function) == identity && there {
            *entry = None; // the page stays leaked until the slot is gone (the controller may still write to it)
        }
    }
}

/// Panel number of the Pi's DSI display touch chip (after the USB panels' numbers).
const DSI_PANEL: usize = MAX_PANELS;

#[cfg(target_arch = "aarch64")]
fn dsi_present() -> bool {
    crate::dsitouch::present()
}
#[cfg(not(target_arch = "aarch64"))]
fn dsi_present() -> bool {
    false
}

/// Reads the DSI chip if there is one (it has no interrupts, so whoever wants events asks).
fn poll_dsi() {
    #[cfg(target_arch = "aarch64")]
    crate::dsitouch::poll(DSI_PANEL as u8);
}

pub fn count() -> usize {
    unsafe { (*core::ptr::addr_of!(PANELS)).iter().flatten().filter(|panel| panel.pointer.kind != hid::Kind::Mouse).count() + usize::from(dsi_present()) + usize::from(mouse_present()) }
}

fn mouse_present() -> bool {
    PS2_MOUSE.load(core::sync::atomic::Ordering::Relaxed) || unsafe { (*core::ptr::addr_of!(PANELS)).iter().flatten().any(|panel| panel.pointer.kind == hid::Kind::Mouse) }
}

/// The PS/2 mouse found (or not) by `keyboard::init_aux`.
pub fn set_ps2_mouse(present: bool) {
    PS2_MOUSE.store(present, core::sync::atomic::Ordering::Relaxed);
}

/// A mouse moved by (`dx`, `dy`) pixels-ish (screen down is positive Y) with the left button `down`. Moves the shared cursor
/// (clamped to the screen, a little acceleration) and queues what a tape needs: the press, the drag, the release and, with
/// the button up, a hover move so a tape can draw the cursor. A screen without pixels (BIOS text mode) ignores it.
pub fn mouse_input(dx: i32, dy: i32, down: bool) {
    let Some((width, height)) = crate::framebuffer::gfx_info() else { return };
    let (width, height) = (width as i32, height as i32);
    MOUSE_PACKETS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    // A touchpad reports a few counts per swipe: quadruple the slow moves, more for fast ones.
    let speed = |d: i32| 4 * d + d * d.abs() / 2;
    unsafe {
        let cursor = &mut *core::ptr::addr_of_mut!(CURSOR);
        if cursor.0 < 0 {
            *cursor = (width / 2, height / 2);
        }
        let before = (*cursor, BUTTON);
        *cursor = ((cursor.0 + speed(dx)).clamp(0, width - 1), (cursor.1 + speed(dy)).clamp(0, height - 1));
        BUTTON = down;
        if before == (*cursor, BUTTON) {
            return;
        }
        let event = TouchEvent { panel: MOUSE_PANEL as u8, id: MOUSE_ID, x: cursor.0, y: cursor.1, down };
        // Moves while the button state stays the same replace the newest queued move, so a fast mouse cannot flood the queue.
        let last = (HEAD + EVENTS - 1) % EVENTS;
        let queue = &mut *core::ptr::addr_of_mut!(QUEUE);
        if HEAD != TAIL && before.1 == down && queue[last].panel == MOUSE_PANEL as u8 && queue[last].down == down {
            queue[last] = event;
        } else {
            queue_push(event);
        }
    }
}

/// For drivers without a USB report pipe (the DSI touch chip).
pub fn push_event(event: TouchEvent) {
    queue_push(event);
}

/// The next finger event.
pub fn take() -> Option<TouchEvent> {
    poll_dsi();
    unsafe {
        if TAIL == HEAD {
            return None;
        }
        let event = (*core::ptr::addr_of!(QUEUE))[TAIL];
        TAIL = (TAIL + 1) % EVENTS;
        Some(event)
    }
}

/// The largest X and Y panel `index` reports (its coordinates run from 0).
pub fn range(index: usize) -> Option<(i32, i32)> {
    if index == MOUSE_PANEL {
        return mouse_present().then(|| crate::framebuffer::gfx_info()).flatten().map(|(width, height)| (width as i32, height as i32));
    }
    #[cfg(target_arch = "aarch64")]
    if index == DSI_PANEL && dsi_present() {
        return Some((crate::dsitouch::WIDTH, crate::dsitouch::HEIGHT));
    }
    let panel = unsafe { (*core::ptr::addr_of!(PANELS)).get(index)?.as_ref()? };
    Some(extent(&panel.pointer))
}

/// How the monitor a panel sits on is turned (`framebuffer::Rotate`; tapes draw upright on the logical screen). Only panels
/// whose screen is known follow a rotation.
fn turn_of(panel: u8) -> Option<crate::framebuffer::Rotate> {
    if usize::from(panel) == MOUSE_PANEL {
        return None; // the cursor already lives on the logical screen
    }
    usize::try_from(screen_of(panel)).ok().filter(|&screen| screen <= 1).and_then(crate::framebuffer::rotation)
}

/// A panel's coordinates seen from the logical (rotated) screen: right = clockwise, the logical top-left corner is at the
/// framebuffer's top right; left = anti-clockwise, at its bottom left (the extent's X and Y swap for both); flip = 180 degrees.
fn upright(turn: Option<crate::framebuffer::Rotate>, (max_x, max_y): (i32, i32), (x, y): (i32, i32)) -> ((i32, i32), (i32, i32)) {
    match turn {
        None => ((max_x, max_y), (x, y)),
        Some(crate::framebuffer::Rotate::Right) => ((max_y, max_x), (y, max_x - x)),
        Some(crate::framebuffer::Rotate::Left) => ((max_y, max_x), (max_y - y, x)),
        Some(crate::framebuffer::Rotate::Flip) => ((max_x, max_y), (max_x - x, max_y - y)),
    }
}

/// Tape ABI 24 `touch_info`.
pub extern "C" fn api_touch_info(panel: u32) -> u64 {
    match range(panel as usize) {
        Some(extent) => {
            let ((x, y), _) = upright(u8::try_from(panel).ok().and_then(turn_of), extent, (0, 0));
            ((x as u64) << 32) | y as u64
        }
        None => 0,
    }
}

/// The tape screen a panel sits on, or -1 when not known (a USB touch screen could be on any monitor): the DSI display is
/// display 0, which is tape screen 0.
fn screen_of(panel: u8) -> i32 {
    if usize::from(panel) == DSI_PANEL || usize::from(panel) == MOUSE_PANEL { 0 } else { -1 }
}

/// Tape ABI 24 `touch_read`: `out` points at six i32 (panel, id, x, y, down, screen).
pub extern "C" fn api_touch_read(out: *mut i32) -> i32 {
    // The USB event rings are only drained when something polls; a tape's wait loop does not, so do it here.
    crate::midishell::poll_usb();
    match take() {
        Some(event) => {
            let extent = range(usize::from(event.panel)).unwrap_or((1, 1));
            let (_, (x, y)) = upright(turn_of(event.panel), extent, (event.x, event.y));
            unsafe { out.copy_from_nonoverlapping([i32::from(event.panel), i32::from(event.id), x, y, i32::from(event.down), screen_of(event.panel)].as_ptr(), 6) };
            1
        }
        None => 0,
    }
}

fn usb_mice() -> usize {
    unsafe { (*core::ptr::addr_of!(PANELS)).iter().flatten().filter(|panel| panel.pointer.kind == hid::Kind::Mouse).count() }
}

pub fn describe_devices(print: impl Fn(core::fmt::Arguments)) {
    for (number, panel) in unsafe { (*core::ptr::addr_of!(PANELS)).iter().enumerate().filter_map(|(number, entry)| Some((number, entry.as_ref()?))).filter(|(_, panel)| panel.pointer.kind != hid::Kind::Mouse) } {
        let (x, y) = extent(&panel.pointer);
        print(format_args!("Touch {}: {:04x}:{:04x} on port {}, {x} x {y}, {} report(s), {} event(s), {} error(s) (last code {})", number + 1, panel.vendor, panel.product, panel.device.path(), panel.reports, panel.events, panel.errors, panel.last_code));
        for index in 0..xhci::count() {
            let Some(controller) = xhci::controller(index) else { continue };
            if xhci::debug() && (controller.pci.bus, controller.pci.device, controller.pci.function) == (panel.bus, panel.device_number, panel.function) {
                let (state, dequeue, enqueue) = controller.endpoint_debug(panel.device.slot, panel.dci);
                print(format_args!("  endpoint state {state}, dequeue {dequeue:#x}, next TRB {enqueue:#x}"));
            }
        }
    }
    if mouse_present() {
        let (x, y) = unsafe { *core::ptr::addr_of!(CURSOR) };
        print(format_args!("Touch {}: mouse cursor (PS/2 {}, USB {}), at {x},{y}, {} packet(s)", MOUSE_PANEL + 1, if PS2_MOUSE.load(core::sync::atomic::Ordering::Relaxed) { "yes" } else { "no" }, usb_mice(), MOUSE_PACKETS.load(core::sync::atomic::Ordering::Relaxed)));
    }
    #[cfg(target_arch = "aarch64")]
    if dsi_present() {
        print(format_args!("Touch {}: DSI display (FT5406 at 0x38), {} x {}, {} read(s)", DSI_PANEL + 1, crate::dsitouch::WIDTH, crate::dsitouch::HEIGHT, crate::dsitouch::reports()));
    }
    let dropped = unsafe { DROPPED };
    if dropped > 0 {
        print(format_args!("Touch: {dropped} event(s) dropped (nobody was reading)"));
    }
}

/// `touch` lists the touch panels; `touch monitor` prints each finger event until Escape.
pub fn command<'a>(words: &mut impl Iterator<Item = &'a str>) {
    match words.next() {
        None => {
            describe_devices(|arguments| crate::println!("{}", arguments));
            if count() == 0 {
                crate::println!("touch: no USB touch panel (`usb rescan` looks again).");
            }
        }
        Some("monitor") => {
            if count() == 0 {
                crate::println!("touch: no USB touch panel (`usb rescan` looks again).");
                return;
            }
            crate::println!("touch: showing finger events; Escape stops.");
            crate::netfs::blocking(true);
            while !crate::netfs::break_requested() {
                crate::midishell::poll_usb();
                crate::storage::pet();
                crate::netfs::drain(|_, _| {}); // a `term net` client's Escape (and our output to it) goes through here
                while let Some(event) = take() {
                    let (x, y) = range(usize::from(event.panel)).unwrap_or((0, 0));
                    crate::println!("touch {} finger {} {} at {},{} (of {x},{y})", event.panel + 1, event.id, if event.down { "down/moved" } else { "up" }, event.x, event.y);
                }
                core::hint::spin_loop();
            }
            crate::netfs::blocking(false);
        }
        Some(_) => crate::println!("Usage: touch | touch monitor"),
    }
}

fn report(vendor: u16, product: u16, interface: u8, descriptor: &[u8]) {
    crate::println!("USB HID {vendor:04x}:{product:04x} interface {interface}: report descriptor, {} bytes:", descriptor.len());
    // A multi-touch panel's descriptor repeats one contact block ten times: the start is enough, and a long dump
    // overflows a term net session so the lines after it are lost.
    for row in descriptor[..descriptor.len().min(96)].chunks(24) {
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
