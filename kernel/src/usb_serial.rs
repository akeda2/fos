//! USB serial adapters: the generic layer. A *chip driver* (`usb_serial_ti.rs` so far) knows one family of
//! adapter chips; this file knows how to talk to a device over either host controller (EHCI or xHCI), keeps the
//! receive ring and the statistics, and presents the ports to `uart.rs`, which numbers them after the built-in
//! 16550 UARTs so that `serial_*` calls, `fterm` and friends work on them unchanged.
//!
//! What a chip driver has to supply (the `Chip` trait): whether it claims a vendor/product, which configuration
//! to select, what to do once the device is configured (a firmware download, say), and how to open, close,
//! set the modem lines and break. Bytes on the bulk endpoints are taken to be raw serial data (true for the TI
//! TUSB3410 and for FTDI-style chips that frame nothing; a chip that frames its data would filter in `rx`).
//!
//! Receiving is polled: `poll()` (called wherever `uart::poll` is) does one bulk IN transfer per open port. These
//! are blocking transfers; the chip must therefore make an idle IN end quickly with a short packet (the TI chip's
//! timeout mode does), otherwise an empty IN costs the host controller's transfer timeout.

use crate::{ehci, memory, xhci};
use fosfor_abi::tape::{SerialInfo, SERIAL_BAD_SETTINGS, SERIAL_KIND_USB};

const RING: usize = 4096;
const MAX_PORTS: usize = 4;
/// Bytes handed to one bulk OUT transfer (the bounce page is 4096; the chip's FIFO is far smaller).
const WRITE_CHUNK: usize = 256;
/// Consecutive failed transfers after which a port is given up on (an unplugged adapter).
const MAX_ERRORS: u32 = 20;
/// How long one poll of an idle EHCI IN endpoint waits (10 ms ticks).
const POLL_TICKS: u64 = 1;
const IN_OFFSET: u64 = 2048;

/// What a chip driver says after a device was configured.
pub enum Attached {
    /// A serial port is ready to be opened.
    Ready,
    /// The device changed personality (a firmware download): it must be reset and enumerated again.
    Reprobe,
    /// It did not work; the text says why.
    Failed(&'static str),
}

pub trait Chip: Sync {
    fn name(&self) -> &'static str;
    fn claims(&self, vendor: u16, product: u16) -> bool;
    /// Index (for GET_DESCRIPTOR) of the configuration to select, given how many the device has.
    fn configuration_index(&self, configurations: u8) -> u8;
    /// The device is configured and `pipe` has its bulk endpoints (`in_ep` is 0 when it has none).
    fn attach(&self, pipe: &mut Pipe, vendor: u16, product: u16) -> Attached;
    fn open(&self, pipe: &mut Pipe, baud: u32, format: u32) -> bool;
    fn close(&self, pipe: &mut Pipe);
    fn set_lines(&self, pipe: &mut Pipe, lines: u32);
    fn set_break(&self, pipe: &mut Pipe, on: bool);
}

static CHIPS: [&dyn Chip; 1] = [&crate::usb_serial_ti::Ti];

#[derive(Clone, Copy)]
enum Link {
    Ehci { controller: usize, target: ehci::Target, root_port: u8 },
    Xhci { controller: usize, device: xhci::Device },
}

/// One attached device's control pipe and bulk endpoints, with a DMA page the data is bounced through: the first half
/// for OUT, the second half (`IN_OFFSET`) for IN.
pub struct Pipe {
    link: Link,
    pub in_ep: u8,
    pub out_ep: u8,
    in_max: u16,
    out_max: u16,
    in_toggle: bool,
    out_toggle: bool,
    bounce: u64,
}

impl Pipe {
    /// A vendor request to the device (bmRequestType 0x40) with up to 64 bytes of data.
    pub fn vendor_out(&mut self, request: u8, value: u16, index: u16, data: &[u8]) -> bool {
        let mut copy = [0u8; 64];
        let length = data.len().min(copy.len());
        copy[..length].copy_from_slice(&data[..length]);
        match self.link {
            Link::Ehci { controller, target, .. } => ehci::controller(controller).is_some_and(|c| c.control_transfer(&target, 0x40, request, value, index, &mut copy[..length]).is_ok()),
            Link::Xhci { controller, device } => xhci::controller(controller).is_some_and(|c| c.control_transfer(&device, 0x40, request, value, index, &mut copy[..length]).is_ok()),
        }
    }

    /// Sends `data` (at most 2048 bytes) on the bulk OUT endpoint. Returns how many bytes went.
    pub fn bulk_out(&mut self, data: &[u8]) -> Result<usize, ()> {
        let length = data.len().min(IN_OFFSET as usize);
        if length == 0 || self.out_ep == 0 {
            return Ok(0);
        }
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), self.bounce as *mut u8, length) };
        self.bulk(false, length)
    }

    /// One bulk IN transfer of at most one packet into the bounce page. Returns the byte count.
    fn bulk_in(&mut self) -> Result<usize, ()> {
        if self.in_ep == 0 {
            return Err(());
        }
        let length = usize::from(self.in_max).clamp(1, 512);
        // An idle adapter NAKs: wait only a tick or two, and treat running out of time as "no data".
        match &mut self.link {
            Link::Ehci { controller, target, .. } => {
                let controller = ehci::controller(*controller).ok_or(())?;
                match controller.bulk_transfer_within(POLL_TICKS, target, self.in_ep & 0x0f, self.in_max, (self.bounce + IN_OFFSET) as u32, length, true, &mut self.in_toggle) {
                    Ok(count) => Ok(count),
                    Err(ehci::Error::Timeout) => Ok(0),
                    Err(_) => Err(()),
                }
            }
            Link::Xhci { .. } => Err(()), // xHCI listens asynchronously: see `drain` and `background_event`
        }
    }

    fn bulk(&mut self, data_in: bool, length: usize) -> Result<usize, ()> {
        let (endpoint, max_packet) = if data_in { (self.in_ep, self.in_max) } else { (self.out_ep, self.out_max) };
        let buffer = self.bounce + if data_in { IN_OFFSET } else { 0 };
        match &mut self.link {
            Link::Ehci { controller, target, .. } => {
                let controller = ehci::controller(*controller).ok_or(())?;
                let toggle = if data_in { &mut self.in_toggle } else { &mut self.out_toggle };
                controller.bulk_transfer(target, endpoint & 0x0f, max_packet, buffer as u32, length, data_in, toggle).map_err(|_| ())
            }
            Link::Xhci { controller, device } => {
                let controller = xhci::controller(*controller).ok_or(())?;
                controller.bulk_transfer(device, endpoint, buffer, length as u32, max_packet).map_err(|_| ())
            }
        }
    }

    /// Clears a halt on both bulk endpoints and resets the data toggles (the chip expects DATA0 after open).
    pub fn clear_halts(&mut self) {
        for endpoint in [self.in_ep, self.out_ep] {
            if endpoint == 0 {
                continue;
            }
            match &mut self.link {
                Link::Ehci { controller, target, .. } => {
                    if let Some(controller) = ehci::controller(*controller) {
                        let _ = controller.control_transfer(target, 0x02, 0x01, 0, u16::from(endpoint), &mut []);
                    }
                }
                Link::Xhci { controller, device } => {
                    if let Some(controller) = xhci::controller(*controller) {
                        controller.clear_endpoint_halt(device, endpoint);
                    }
                }
            }
        }
        self.in_toggle = false;
        self.out_toggle = false;
    }
}

struct Port {
    pipe: Pipe,
    chip: &'static dyn Chip,
    open: bool,
    failed: bool,
    /// xHCI: an IN transfer is queued and its event has not arrived yet.
    listening: bool,
    /// IN transfers that completed (empty ones included): tells a quiet line from a stuck receive path.
    completions: u64,
    errors: u32,
    baud: u32,
    format: u32,
    lines: u32,
    ring: [u8; RING],
    head: usize,
    tail: usize,
    rx_bytes: u64,
    tx_bytes: u64,
    overruns: u32,
}

static mut PORTS: [Option<Port>; MAX_PORTS] = [const { None }; MAX_PORTS];
/// Set when a device must be enumerated again (after a firmware download); `usb_hid::probe_all` reads it.
static mut REPROBE: bool = false;

fn ports() -> &'static mut [Option<Port>; MAX_PORTS] {
    unsafe { &mut *core::ptr::addr_of_mut!(PORTS) }
}

fn port(index: usize) -> Option<&'static mut Port> {
    ports().get_mut(index).and_then(|slot| slot.as_mut())
}

pub fn take_reprobe() -> bool {
    unsafe { core::mem::replace(&mut *core::ptr::addr_of_mut!(REPROBE), false) }
}

/// How many port numbers USB serial takes: up to the last occupied slot (an unplugged adapter leaves a hole, so the others keep their numbers).
pub fn count() -> usize {
    ports().iter().rposition(|slot| slot.is_some()).map_or(0, |last| last + 1)
}

pub fn any_open() -> bool {
    ports().iter().flatten().any(|port| port.open)
}

/// The first bulk endpoints of the first interface that has any: (in, in max packet, out, out max packet, burst bytes ignored).
fn find_bulk(config: &[u8]) -> (u8, u16, u8, u16, u8, u8) {
    let (mut in_ep, mut in_max, mut out_ep, mut out_max, mut in_burst, mut out_burst) = (0u8, 0u16, 0u8, 0u16, 0u8, 0u8);
    let mut at = 0;
    let mut last_in = false;
    while at + 1 < config.len() {
        let length = usize::from(config[at]);
        if length < 2 || at + length > config.len() {
            break;
        }
        match config[at + 1] {
            4 if in_ep != 0 || out_ep != 0 => break, // the first interface with bulk endpoints is complete
            5 if length >= 7 && config[at + 3] & 3 == 2 => {
                let address = config[at + 2];
                let max = u16::from_le_bytes([config[at + 4], config[at + 5]]) & 0x7ff;
                last_in = address & 0x80 != 0;
                if last_in && in_ep == 0 {
                    (in_ep, in_max) = (address, max);
                } else if !last_in && out_ep == 0 {
                    (out_ep, out_max) = (address, max);
                }
            }
            0x30 if length >= 3 => {
                if last_in {
                    in_burst = config[at + 2];
                } else {
                    out_burst = config[at + 2];
                }
            }
            _ => {}
        }
        at += length;
    }
    (in_ep, in_max, out_ep, out_max, in_burst, out_burst)
}

/// How the attach went, for the hook in `usb_hid`.
pub enum Outcome {
    /// Not ours.
    Unclaimed,
    /// A port exists now.
    Attached,
    /// The device needs a reset and a fresh enumeration (see `take_reprobe`).
    Reprobe,
    /// A chip driver claims it but could not attach (the firmware file is missing, say): look again at the next `usb rescan`.
    Retry,
}

/// Which configuration descriptor index a device with this ID wants, if a chip driver claims it.
pub fn configuration_index(vendor: u16, product: u16, configurations: u8) -> Option<u8> {
    CHIPS.iter().find(|chip| chip.claims(vendor, product)).map(|chip| chip.configuration_index(configurations))
}

fn finish(link: Link, config: &[u8], vendor: u16, product: u16) -> Outcome {
    let Some(chip) = CHIPS.iter().find(|chip| chip.claims(vendor, product)) else { return Outcome::Unclaimed };
    let (in_ep, in_max, out_ep, out_max, in_burst, out_burst) = find_bulk(config);
    if out_ep == 0 {
        crate::println!("USB serial: {vendor:04x}:{product:04x}: no bulk OUT endpoint.");
        return Outcome::Unclaimed;
    }
    if let Link::Xhci { controller, device } = link {
        let Some(controller) = xhci::controller(controller) else { return Outcome::Unclaimed };
        if let Err(error) = controller.configure_bulk_endpoints(&device, in_ep, in_max, in_burst, out_ep, out_max, out_burst) {
            crate::println!("USB serial: configuring the bulk endpoints failed ({error:?}).");
            return Outcome::Unclaimed;
        }
    }
    let Some(bounce) = memory::allocate_contiguous(1, Some(1 << 32)) else { return Outcome::Unclaimed };
    let mut pipe = Pipe { link, in_ep, out_ep, in_max, out_max, in_toggle: false, out_toggle: false, bounce };
    match chip.attach(&mut pipe, vendor, product) {
        Attached::Reprobe => {
            unsafe { *core::ptr::addr_of_mut!(REPROBE) = true };
            Outcome::Reprobe
        }
        Attached::Failed(why) => {
            crate::println!("USB serial: {}: {why}.", chip.name());
            Outcome::Retry
        }
        Attached::Ready => {
            let Some(slot) = ports().iter_mut().find(|slot| slot.is_none()) else { return Outcome::Unclaimed };
            *slot = Some(Port { pipe, chip: *chip, open: false, failed: false, listening: false, completions: 0, errors: 0, baud: 0, format: 0, lines: 0, ring: [0; RING], head: 0, tail: 0, rx_bytes: 0, tx_bytes: 0, overruns: 0 });
            Outcome::Attached
        }
    }
}

/// An EHCI device (already addressed; `root_port` is its root port, or 0 behind a hub) that a chip driver may claim.
pub fn attach_ehci(controller_index: usize, controller: &mut ehci::Controller, target: ehci::Target, root_port: u8, vendor: u16, product: u16, configurations: u8) -> Outcome {
    let Some(index) = configuration_index(vendor, product, configurations) else { return Outcome::Unclaimed };
    let mut config = [0u8; 256];
    let Ok(_) = controller.get_descriptor(&target, 2, index, &mut config[..9]) else { return Outcome::Unclaimed };
    let total = usize::from(u16::from_le_bytes([config[2], config[3]])).clamp(9, config.len());
    let Ok(_) = controller.get_descriptor(&target, 2, index, &mut config[..total]) else { return Outcome::Unclaimed };
    if controller.set_configuration(&target, config[5]).is_err() {
        return Outcome::Unclaimed;
    }
    finish(Link::Ehci { controller: controller_index, target, root_port }, &config[..total], vendor, product)
}

/// The same for an xHCI device with a slot.
pub fn attach_xhci(controller_index: usize, controller: &mut xhci::Controller, device: xhci::Device, vendor: u16, product: u16, configurations: u8) -> Outcome {
    let Some(index) = configuration_index(vendor, product, configurations) else { return Outcome::Unclaimed };
    let mut config = [0u8; 256];
    let Ok(_) = controller.get_descriptor(&device, 2, index, &mut config[..9]) else { return Outcome::Unclaimed };
    let total = usize::from(u16::from_le_bytes([config[2], config[3]])).clamp(9, config.len());
    let Ok(_) = controller.get_descriptor(&device, 2, index, &mut config[..total]) else { return Outcome::Unclaimed };
    if controller.set_configuration(&device, config[5]).is_err() {
        return Outcome::Unclaimed;
    }
    finish(Link::Xhci { controller: controller_index, device }, &config[..total], vendor, product)
}

/// A device behind hub `hub_address` went away (`hub_port` 0: everything behind the hub): drop its ports.
pub fn ehci_departed(controller_index: usize, hub_address: u8, hub_port: u8) {
    for slot in ports().iter_mut() {
        let gone = matches!(slot, Some(Port { pipe: Pipe { link: Link::Ehci { controller, target, .. }, .. }, .. })
            if *controller == controller_index && hub_address != 0 && target.hub_address == hub_address && (hub_port == 0 || target.hub_port == hub_port));
        if gone {
            *slot = None;
        }
    }
}

/// The device directly on EHCI root port `root_port` went away.
pub fn ehci_root_departed(controller_index: usize, root_port: u8) {
    for slot in ports().iter_mut() {
        let gone = matches!(slot, Some(Port { pipe: Pipe { link: Link::Ehci { controller, root_port: port, .. }, .. }, .. }) if *controller == controller_index && *port == root_port);
        if gone {
            *slot = None;
        }
    }
}

pub fn xhci_departed(controller_index: usize, root_port: u8) {
    for slot in ports().iter_mut() {
        let gone = matches!(slot, Some(Port { pipe: Pipe { link: Link::Xhci { controller, device }, .. }, .. }) if *controller == controller_index && device.port == root_port);
        if gone {
            *slot = None;
        }
    }
}

/// One line per port for the `usb` command.
pub fn describe() {
    for (index, port) in ports().iter().enumerate() {
        if let Some(port) = port {
            crate::println!("USB serial: USB{} is a {}{}; {} IN transfers completed (xHCI).", index + 1, port.chip.name(), if port.failed { " (stopped answering)" } else { "" }, port.completions);
        }
    }
}

fn push_received(port: &mut Port, count: usize) {
    let data = (port.pipe.bounce + IN_OFFSET) as *const u8;
    for offset in 0..count {
        let byte = unsafe { data.add(offset).read() };
        let next = (port.head + 1) % RING;
        if next == port.tail {
            port.overruns += 1;
            continue;
        }
        port.ring[port.head] = byte;
        port.head = next;
        port.rx_bytes += 1;
    }
}

/// Moves what the device has into the ring: one bulk IN per call on EHCI; on xHCI it makes sure an IN is queued (the
/// completion is `background_event`'s).
fn drain(port: &mut Port) {
    if port.failed {
        return;
    }
    if let Link::Xhci { controller, device } = port.pipe.link {
        if !port.listening {
            if let Some(controller) = xhci::controller(controller) {
                let dci = u32::from(port.pipe.in_ep & 0x0f) * 2 + 1;
                // A Normal TRB with IOC and ISP: right for a bulk IN too.
                controller.queue_interrupt_in(device.slot, dci, port.pipe.bounce + IN_OFFSET, u32::from(port.pipe.in_max).clamp(1, 512));
                port.listening = true;
            }
        }
        return;
    }
    match port.pipe.bulk_in() {
        Ok(count) => {
            port.errors = 0;
            push_received(port, count);
        }
        Err(()) => {
            port.errors += 1;
            if port.errors >= MAX_ERRORS {
                port.failed = true;
                crate::println!("USB serial: the adapter stopped answering; closing its port.");
            }
        }
    }
}

/// An xHCI IN transfer finished: take the bytes, and listen again while the port is open. False if the event is not ours.
pub fn background_event(controller: &mut xhci::Controller, event: &xhci::Trb) -> bool {
    let identity = (controller.pci.bus, controller.pci.device, controller.pci.function);
    for port in ports().iter_mut().flatten() {
        let Link::Xhci { controller: index, device } = port.pipe.link else { continue };
        let dci = u32::from(port.pipe.in_ep & 0x0f) * 2 + 1;
        let Some(owner) = xhci::controller(index) else { continue };
        if (owner.pci.bus, owner.pci.device, owner.pci.function) != identity || device.slot != event.slot_number() || dci != event.endpoint_index() {
            continue;
        }
        let length = u32::from(port.pipe.in_max).clamp(1, 512);
        port.listening = false;
        port.completions += 1;
        match event.completion() {
            xhci::COMPLETION_SUCCESS | xhci::COMPLETION_SHORT => {
                port.errors = 0;
                if port.open {
                    push_received(port, length.saturating_sub(event.residual()) as usize);
                }
            }
            _ => {
                controller.reset_stalled_endpoint(device.slot, dci);
                port.errors += 1;
                if port.errors >= MAX_ERRORS {
                    port.failed = true;
                }
            }
        }
        if port.open && !port.failed {
            controller.queue_interrupt_in(device.slot, dci, port.pipe.bounce + IN_OFFSET, length);
            port.listening = true;
        }
        return true;
    }
    false
}

pub fn poll() {
    let mut xhci_open = false;
    for port in ports().iter_mut().flatten() {
        if port.open {
            xhci_open |= matches!(port.pipe.link, Link::Xhci { .. });
            drain(port);
        }
    }
    if xhci_open {
        crate::usb_hid::poll(); // delivers the IN completions to `background_event`
    }
}

pub fn info(index: usize, out: &mut SerialInfo) -> bool {
    let Some(port) = port(index) else { return false };
    let mut name = [0u8; 8];
    name[..3].copy_from_slice(b"USB");
    name[3] = b'1' + index as u8;
    *out = SerialInfo {
        kind: SERIAL_KIND_USB,
        open: u32::from(port.open),
        io_base: 0,
        baud: port.baud,
        format: port.format,
        rx_bytes: port.rx_bytes,
        tx_bytes: port.tx_bytes,
        overruns: port.overruns,
        framing_errors: 0,
        parity_errors: 0,
        breaks: 0,
        name,
    };
    true
}

pub fn open(index: usize, baud: u32, format: u32) -> i32 {
    let Some(port) = port(index) else { return fosfor_abi::tape::SERIAL_NO_PORT };
    if baud == 0 || baud > 115_200 || format & !0x1f != 0 || port.failed {
        return SERIAL_BAD_SETTINGS;
    }
    if !port.chip.open(&mut port.pipe, baud, format) {
        return SERIAL_BAD_SETTINGS;
    }
    port.baud = baud;
    port.format = format;
    port.lines = fosfor_abi::tape::SERIAL_LINE_DTR | fosfor_abi::tape::SERIAL_LINE_RTS;
    port.open = true;
    (port.head, port.tail, port.errors) = (0, 0, 0);
    (port.rx_bytes, port.tx_bytes, port.overruns) = (0, 0, 0);
    0
}

pub fn close(index: usize) {
    if let Some(port) = port(index) {
        if port.open {
            port.open = false;
            port.lines = 0;
            port.chip.close(&mut port.pipe);
        }
    }
}

pub fn read(index: usize, out: &mut [u8]) -> usize {
    let Some(port) = port(index).filter(|port| port.open) else { return 0 };
    drain(port);
    let mut count = 0;
    while count < out.len() && port.tail != port.head {
        out[count] = port.ring[port.tail];
        port.tail = (port.tail + 1) % RING;
        count += 1;
    }
    count
}

pub fn write(index: usize, data: &[u8]) -> usize {
    let Some(port) = port(index).filter(|port| port.open && !port.failed) else { return 0 };
    let data = &data[..data.len().min(WRITE_CHUNK)];
    match port.pipe.bulk_out(data) {
        Ok(count) => {
            port.tx_bytes += count as u64;
            count
        }
        Err(()) => {
            port.errors += 1;
            0
        }
    }
}

pub fn lines(index: usize) -> u32 {
    port(index).filter(|port| port.open).map_or(0, |port| port.lines)
}

pub fn set_lines(index: usize, lines: u32) {
    if let Some(port) = port(index).filter(|port| port.open) {
        port.lines = lines;
        port.chip.set_lines(&mut port.pipe, lines);
    }
}

pub fn set_break(index: usize, on: bool) {
    if let Some(port) = port(index).filter(|port| port.open) {
        port.chip.set_break(&mut port.pipe, on);
    }
}
