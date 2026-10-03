//! An EHCI (USB 2.0 high-speed) host controller driver, built for keyboards: it
//! enumerates root ports (and one level of hub behind them), runs control transfers,
//! and keeps one interrupt-IN endpoint per keyboard armed. Milestone M4 extends it to
//! bulk storage.
//!
//! Why EHCI matters: on Intel 6-series (Sandy Bridge) boards every EHCI root port is
//! really an integrated high-speed *hub* (the "rate matching hub"), and anything
//! plugged in -- every low-speed keyboard -- sits behind it. A high-speed hub talks to
//! a full/low-speed device through a *transaction translator* (TT), driven by **split
//! transactions**: the host sends a "start-split" to the hub saying "do this transaction
//! at device D, port P", later a "complete-split" asking for the result. The EHCI
//! controller does the splitting itself once the queue head (QH) says which hub and
//! port the device is behind and what speed it is, so for this driver "split
//! transaction support" is mostly filling those QH fields correctly (see `Target`).
//!
//! EHCI in one paragraph. The controller walks *schedules* of descriptors in RAM. The
//! *asynchronous* schedule is a circular list of QHs. A QH names one endpoint (device
//! address, endpoint number, speed, max packet) and holds an *overlay*, a working copy
//! of the queue transfer descriptor (qTD) it is executing, plus a pointer to the next
//! qTD. A qTD is one chunk of a transfer: PID (SETUP/IN/OUT), byte count, data toggle,
//! buffer pointers and a status byte the controller rewrites (Active clears when done;
//! Halted and error bits say why not).
//!
//! This driver's async list is a small ring: one QH reprogrammed for every control
//! transfer (with the schedule switched off while it is) plus one permanent QH per
//! keyboard. A keyboard's interrupt endpoint is polled through the async schedule
//! rather than the periodic one: the device just NAKs until it has a report and the
//! controller keeps retrying. That wastes bus time a periodic schedule would not, but
//! needs no frame list or microframe masks, and a keyboard does not mind.
//! Full- and low-speed devices wired straight to a root port belong to the companion
//! controller (UHCI/OHCI), which this kernel has no driver for: such a port is
//! released to it. Polling only; no interrupts.

use crate::{apic, memory, paging, pci};

pub const MAX_CONTROLLERS: usize = 2;
pub const MAX_KEYBOARDS: usize = 2;
const MAX_QTDS: usize = 8;
const TIMEOUT_TICKS: u64 = 100;
const QTD_MAX_BYTES: usize = 16 * 1024;

const CAP_HCSPARAMS: usize = 0x04;
const CAP_HCCPARAMS: usize = 0x08;
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_USBINTR: usize = 0x08;
const OP_CTRLDSSEGMENT: usize = 0x10;
const OP_ASYNCLISTADDR: usize = 0x18;
const OP_CONFIGFLAG: usize = 0x40;
const OP_PORTSC: usize = 0x44;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_ASYNC_ENABLE: u32 = 1 << 5;
const STS_HALTED: u32 = 1 << 12;
const STS_ASYNC_RUNNING: u32 = 1 << 15;

const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 2;
const PORT_RESET: u32 = 1 << 8;
const PORT_POWER: u32 = 1 << 12;
const PORT_OWNER: u32 = 1 << 13;
const PORT_CHANGE_BITS: u32 = (1 << 1) | (1 << 3) | (1 << 5);
const PORT_KEEP: u32 = PORT_ENABLED | PORT_POWER | PORT_OWNER | (0x3 << 14) | (0xf << 16) | (0x7 << 20);

const TOKEN_ACTIVE: u32 = 1 << 7;
const TOKEN_HALTED: u32 = 1 << 6;
const PID_OUT: u32 = 0;
const PID_IN: u32 = 1;
const PID_SETUP: u32 = 2;
const TERMINATE: u32 = 1;
const LINK_QH: u32 = 1 << 1;

// One DMA frame per controller (below 4 GiB: EHCI pointers are 32-bit).
const OFFSET_CONTROL_QH: usize = 0x000;
const OFFSET_KEYBOARD_QH: usize = 0x040; // + 0x40 per keyboard
const OFFSET_QTDS: usize = 0x100; // MAX_QTDS control qTDs, 0x40 apart
const OFFSET_KEYBOARD_QTD: usize = 0x500; // + 0x40 per keyboard
const OFFSET_KEYBOARD_BUFFER: usize = 0x600; // + 0x20 per keyboard
const OFFSET_SETUP: usize = 0xb00;
const OFFSET_BOUNCE: usize = 0xc00;
const MAX_CONTROL_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy)]
pub enum Error {
    Timeout,
    NoDevice,
    /// A full/low-speed device on a root port: handed to the companion controller.
    CompanionPort,
    /// A full/low-speed device behind a hub that has no transaction translator.
    NoTranslator,
    Transfer(u8),
    NoMemory,
    Unsupported,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Speed {
    Full = 0,
    Low = 1,
    High = 2,
}

/// Everything the QH needs to address one device's endpoints, including (for a
/// full/low-speed device) which hub's transaction translator carries the split
/// transactions: the hub's USB address and the port the device is plugged into.
#[derive(Clone, Copy)]
pub struct Target {
    pub address: u8,
    pub max_packet0: u16,
    pub speed: Speed,
    pub hub_address: u8,
    pub hub_port: u8,
}

impl Target {
    const DEFAULT: Target = Target { address: 0, max_packet0: 64, speed: Speed::High, hub_address: 0, hub_port: 0 };
}

/// QH dword 1: device address, endpoint, endpoint speed, data toggle from the qTD,
/// head-of-list flag, max packet size, and the "control endpoint" flag that a
/// non-high-speed control endpoint must carry.
fn qh_characteristics(target: &Target, endpoint: u8, max_packet: u16, head: bool, control: bool) -> u32 {
    u32::from(target.address)
        | (u32::from(endpoint & 0xf) << 8)
        | ((target.speed as u32) << 12)
        | (1 << 14)
        | (u32::from(head) << 15)
        | (u32::from(max_packet) << 16)
        | (u32::from(control && target.speed != Speed::High) << 27)
}

/// QH dword 2: one transaction per microframe, and the transaction translator's hub
/// address and port (zero for a high-speed device). The microframe masks are zero:
/// for asynchronous split transactions the controller schedules them itself.
fn qh_capabilities(target: &Target) -> u32 {
    (1 << 30) | (u32::from(target.hub_port) << 23) | (u32::from(target.hub_address) << 16)
}

fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}
fn write32(address: usize, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) }
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = apic::ticks();
    loop {
        crate::debug::pet();
        if condition() {
            return true;
        }
        if apic::ticks().wrapping_sub(start) >= ticks {
            return condition();
        }
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy)]
struct Qtd {
    pid: u32,
    toggle: bool,
    bytes: u32,
    buffer: u32,
    /// Where a short packet continues (the status stage of a control read).
    alternate: Option<usize>,
}

/// A device found by enumeration.
#[derive(Clone, Copy)]
pub struct Found {
    pub target: Target,
    pub descriptor: [u8; 18],
}

#[derive(Clone, Copy)]
struct KeyboardPipe {
    target: Target,
    endpoint: u8,
    max_packet: u16,
    toggle: bool,
}

pub struct Controller {
    pub pci: pci::Device,
    operational: usize,
    pub ports: u8,
    frame: usize,
    next_address: u8,
    /// The data toggle the controller left in the last completed qTD of the previous chain.
    last_toggle: bool,
    /// Root ports where a keyboard was found (bit n = port n); rescans skip them.
    pub claimed_ports: u16,
    keyboards: [Option<KeyboardPipe>; MAX_KEYBOARDS],
}

static mut CONTROLLERS: [Option<Controller>; MAX_CONTROLLERS] = [const { None }; MAX_CONTROLLERS];
static mut COUNT: usize = 0;

pub fn count() -> usize {
    unsafe { COUNT }
}

pub fn controller(index: usize) -> Option<&'static mut Controller> {
    unsafe { (*core::ptr::addr_of_mut!(CONTROLLERS)).get_mut(index)?.as_mut() }
}

/// Starts every EHCI controller (PCI class 0x0c, subclass 0x03, interface 0x20).
pub fn init_all() -> usize {
    for device in pci::devices().filter(|d| d.class == 0x0c && d.subclass == 0x03 && d.interface == 0x20) {
        if count() == MAX_CONTROLLERS {
            break;
        }
        match Controller::init(device) {
            Ok(controller) => unsafe {
                CONTROLLERS[COUNT] = Some(controller);
                COUNT += 1;
            },
            Err(error) => crate::println!("EHCI at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    count()
}

/// The firmware handshake: EHCI keeps a "USB Legacy Support" capability in PCI config
/// space at the offset HCCPARAMS.EECP gives. Set the OS-owned semaphore, wait for the
/// BIOS-owned one to clear, switch off the firmware's SMI sources.
fn take_ownership_from_firmware(device: &pci::Device, hccparams: u32) {
    let offset = ((hccparams >> 8) & 0xff) as u8;
    if offset < 0x40 {
        return;
    }
    let header = device.read32(offset);
    if header & 0xff != 1 {
        return;
    }
    const BIOS_OWNED: u32 = 1 << 16;
    const OS_OWNED: u32 = 1 << 24;
    if header & BIOS_OWNED != 0 {
        device.write32(offset, header | OS_OWNED);
        wait_until(100, || device.read32(offset) & BIOS_OWNED == 0);
    }
    device.write32(offset + 4, 0xe000_0000);
}

impl Controller {
    fn init(device: pci::Device) -> Result<Controller, Error> {
        device.force_power_state_d0();
        device.enable_memory_and_bus_master();
        let (base, size) = device.memory_bar(0).ok_or(Error::NoDevice)?;
        paging::map_device(base, size);
        let mmio = base as usize;
        let cap_length = unsafe { (mmio as *const u8).read_volatile() } as usize;
        let hcsparams = read32(mmio + CAP_HCSPARAMS);
        let hccparams = read32(mmio + CAP_HCCPARAMS);
        let operational = mmio + cap_length;
        let ports = (hcsparams & 0xf) as u8;
        if ports == 0 || ports > 15 {
            return Err(Error::Unsupported);
        }
        take_ownership_from_firmware(&device, hccparams);

        let command = read32(operational + OP_USBCMD);
        if command & CMD_RUN != 0 {
            write32(operational + OP_USBCMD, command & !(CMD_RUN | CMD_ASYNC_ENABLE));
            if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED != 0) {
                return Err(Error::Timeout);
            }
        }
        write32(operational + OP_USBCMD, CMD_RESET);
        if !wait_until(100, || read32(operational + OP_USBCMD) & CMD_RESET == 0) {
            return Err(Error::Timeout);
        }
        write32(operational + OP_USBINTR, 0); // polling only

        let frame = memory::allocate_contiguous(1, Some(1 << 32)).ok_or(Error::NoMemory)? as usize;
        if hccparams & 1 != 0 {
            write32(operational + OP_CTRLDSSEGMENT, 0); // 64-bit capable: all our DMA memory is below 4 GiB
        }
        // The async list: control QH -> keyboard QHs -> back to the control QH, which is
        // the head of the reclamation list (H bit). Keyboard QHs start idle: no qTD, not active.
        let qh_at = |index: usize| (frame + if index == 0 { OFFSET_CONTROL_QH } else { OFFSET_KEYBOARD_QH + (index - 1) * 0x40 }) as u32;
        for index in 0..=MAX_KEYBOARDS {
            let words = qh_at(index) as *mut u32;
            let next = qh_at((index + 1) % (MAX_KEYBOARDS + 1));
            unsafe {
                words.write_volatile(next | LINK_QH);
                words.add(1).write_volatile(if index == 0 { 1 << 15 } else { 0 });
                words.add(2).write_volatile(1 << 30);
                words.add(4).write_volatile(TERMINATE);
                words.add(5).write_volatile(TERMINATE);
            }
        }
        write32(operational + OP_ASYNCLISTADDR, qh_at(0));

        write32(operational + OP_USBCMD, CMD_RUN | (8 << 16));
        if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED == 0) {
            return Err(Error::Timeout);
        }
        // Route every port to this controller instead of its companions, then power
        // them (when software controls port power) and let devices settle.
        write32(operational + OP_CONFIGFLAG, 1);
        if hcsparams & (1 << 4) != 0 {
            for port in 1..=ports {
                let register = operational + OP_PORTSC + 4 * (usize::from(port) - 1);
                write32(register, (read32(register) & PORT_KEEP) | PORT_POWER);
            }
        }
        apic::wait_ticks(15);
        Ok(Controller { pci: device, operational, ports, frame, next_address: 1, last_toggle: false, claimed_ports: 0, keyboards: [None; MAX_KEYBOARDS] })
    }

    fn port_register(&self, port: u8) -> usize {
        self.operational + OP_PORTSC + 4 * (usize::from(port) - 1)
    }

    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    pub fn port_has_device(&self, port: u8) -> bool {
        let status = self.port_status(port);
        status & PORT_CONNECTED != 0 && status & PORT_OWNER == 0
    }

    pub fn keyboard_count(&self) -> usize {
        self.keyboards.iter().flatten().count()
    }

    fn set_async(&self, on: bool) -> bool {
        let command = read32(self.operational + OP_USBCMD);
        write32(self.operational + OP_USBCMD, if on { command | CMD_ASYNC_ENABLE } else { command & !CMD_ASYNC_ENABLE });
        wait_until(5, || (read32(self.operational + OP_USBSTS) & STS_ASYNC_RUNNING != 0) == on)
    }

    fn qtd_address(&self, index: usize) -> u32 {
        (self.frame + OFFSET_QTDS + index * 0x40) as u32
    }

    fn write_qtd(address: u32, qtd: &Qtd, next: u32, alternate: u32, interrupt: bool) {
        let token = (u32::from(qtd.toggle) << 31) | (qtd.bytes << 16) | (u32::from(interrupt) << 15) | (3 << 10) | (qtd.pid << 8) | TOKEN_ACTIVE;
        unsafe {
            let words = address as *mut u32;
            words.write_volatile(next);
            words.add(1).write_volatile(alternate);
            words.add(2).write_volatile(token);
            for page in 0..5u32 {
                // Buffer pointer 0 carries the offset within its page; the rest are page-aligned continuations.
                let pointer = if page == 0 { qtd.buffer } else { (qtd.buffer & !0xfff) + page * 0x1000 };
                words.add(3 + page as usize).write_volatile(pointer);
            }
        }
    }

    /// Runs `qtds` as one queue transfer on the control QH. Returns each qTD's
    /// untransferred byte count, or an error if it halted or timed out.
    fn run(&mut self, target: &Target, endpoint: u8, max_packet: u16, control: bool, qtds: &[Qtd]) -> Result<[u32; MAX_QTDS], Error> {
        if !self.set_async(false) {
            return Err(Error::Timeout);
        }
        for (index, qtd) in qtds.iter().enumerate() {
            let next = if index + 1 < qtds.len() { self.qtd_address(index + 1) } else { TERMINATE };
            let alternate = qtd.alternate.map_or(TERMINATE, |at| self.qtd_address(at));
            Self::write_qtd(self.qtd_address(index), qtd, next, alternate, index + 1 == qtds.len());
        }
        unsafe {
            let words = (self.frame + OFFSET_CONTROL_QH) as *mut u32;
            words.add(1).write_volatile(qh_characteristics(target, endpoint, max_packet, true, control));
            words.add(2).write_volatile(qh_capabilities(target));
            words.add(3).write_volatile(0);
            words.add(4).write_volatile(self.qtd_address(0)); // overlay: next qTD
            words.add(5).write_volatile(TERMINATE);
            for word in 6..12 {
                words.add(word).write_volatile(0); // overlay token idle, buffers cleared
            }
        }
        write32(self.operational + OP_USBSTS, 0x3f);
        let outcome = if self.set_async(true) { self.wait_chain(qtds) } else { Err(Error::Timeout) };
        self.set_async(false);
        if self.keyboard_count() > 0 {
            self.set_async(true); // keep the keyboards' queue heads running
        }
        outcome
    }

    fn wait_chain(&mut self, qtds: &[Qtd]) -> Result<[u32; MAX_QTDS], Error> {
        let start = apic::ticks();
        loop {
            let mut remaining = [0u32; MAX_QTDS];
            let mut finished = true;
            for index in 0..qtds.len() {
                let token = unsafe { ((self.qtd_address(index) + 8) as *const u32).read_volatile() };
                remaining[index] = (token >> 16) & 0x7fff;
                if token & TOKEN_HALTED != 0 {
                    return Err(Error::Transfer((token & 0xff) as u8));
                }
                if token & TOKEN_ACTIVE != 0 {
                    finished = false;
                    break;
                }
                self.last_toggle = token >> 31 != 0;
                if remaining[index] != 0 && qtds[index].alternate.is_none() {
                    // A short packet ended this qTD and the queue with it.
                    for later in index + 1..qtds.len() {
                        remaining[later] = qtds[later].bytes;
                    }
                    return Ok(remaining);
                }
            }
            if finished {
                return Ok(remaining);
            }
            if apic::ticks().wrapping_sub(start) >= TIMEOUT_TICKS {
                return Err(Error::Timeout);
            }
        }
    }

    /// One control transfer: SETUP, an optional DATA stage (bounced through the
    /// controller's frame), STATUS. Returns the data bytes moved.
    pub fn control_transfer(&mut self, target: &Target, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8]) -> Result<usize, Error> {
        if data.len() > MAX_CONTROL_BYTES {
            return Err(Error::Unsupported);
        }
        let data_in = request_type & 0x80 != 0;
        let (setup, bounce) = ((self.frame + OFFSET_SETUP) as u32, (self.frame + OFFSET_BOUNCE) as u32);
        unsafe {
            let fields = [request_type, request, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, data.len() as u8, (data.len() >> 8) as u8];
            core::ptr::copy_nonoverlapping(fields.as_ptr(), setup as *mut u8, 8);
            if !data_in && !data.is_empty() {
                core::ptr::copy_nonoverlapping(data.as_ptr(), bounce as *mut u8, data.len());
            }
        }
        let blank = Qtd { pid: 0, toggle: false, bytes: 0, buffer: 0, alternate: None };
        let mut qtds = [blank; 3];
        let mut count = 1;
        qtds[0] = Qtd { pid: PID_SETUP, toggle: false, bytes: 8, buffer: setup, alternate: None };
        let data_stage = !data.is_empty();
        if data_stage {
            // A short IN data stage must still run the status stage: continue there.
            qtds[count] = Qtd { pid: if data_in { PID_IN } else { PID_OUT }, toggle: true, bytes: data.len() as u32, buffer: bounce, alternate: Some(count + 1) };
            count += 1;
        }
        qtds[count] = Qtd { pid: if data_stage && data_in { PID_OUT } else { PID_IN }, toggle: true, bytes: 0, buffer: bounce, alternate: None };
        count += 1;
        let remaining = self.run(target, 0, target.max_packet0, true, &qtds[..count])?;
        let moved = if data_stage { data.len() - remaining[1] as usize } else { 0 };
        if data_in && moved != 0 {
            unsafe { core::ptr::copy_nonoverlapping(bounce as *const u8, data.as_mut_ptr(), moved) };
        }
        Ok(moved)
    }

        /// One bulk transfer between the device and the physical buffer at `buffer` (below
    /// 4 GiB), chunked into qTDs, with the caller's data toggle kept current. Returns
    /// the bytes moved.
    pub fn bulk_transfer(&mut self, target: &Target, endpoint: u8, max_packet: u16, buffer: u32, length: usize, data_in: bool, toggle: &mut bool) -> Result<usize, Error> {
        if length == 0 || length > MAX_QTDS * QTD_MAX_BYTES {
            return Err(Error::Unsupported);
        }
        let blank = Qtd { pid: 0, toggle: false, bytes: 0, buffer: 0, alternate: None };
        let mut qtds = [blank; MAX_QTDS];
        let mut count = 0;
        let mut offset = 0;
        // Each qTD starts with the toggle the packets before it leave behind.
        let mut next_toggle = *toggle;
        while offset < length {
            let bytes = (length - offset).min(QTD_MAX_BYTES);
            qtds[count] = Qtd { pid: if data_in { PID_IN } else { PID_OUT }, toggle: next_toggle, bytes: bytes as u32, buffer: buffer + offset as u32, alternate: None };
            if bytes.div_ceil(usize::from(max_packet.max(1))) % 2 == 1 {
                next_toggle = !next_toggle;
            }
            count += 1;
            offset += bytes;
        }
        let remaining = self.run(target, endpoint, max_packet, false, &qtds[..count])?;
        *toggle = self.last_toggle;
        let left: u32 = remaining[..count].iter().sum();
        Ok(length - left as usize)
    }

    pub fn get_descriptor(&mut self, target: &Target, kind: u8, index: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(target, 0x80, 0x06, (u16::from(kind) << 8) | u16::from(index), 0, buffer)
    }

    pub fn set_configuration(&mut self, target: &Target, value: u8) -> Result<(), Error> {
        self.control_transfer(target, 0x00, 0x09, u16::from(value), 0, &mut []).map(|_| ())
    }

    /// Resets a root port with a device on it and leaves a high-speed device enabled
    /// at address 0. Anything slower is released to the companion controller.
    fn reset_root_port(&mut self, port: u8) -> Result<(), Error> {
        let register = self.port_register(port);
        let status = read32(register);
        if status & PORT_CONNECTED == 0 || status & PORT_OWNER != 0 {
            return Err(Error::NoDevice);
        }
        // Line state "K" (01) is a low-speed device, which EHCI cannot drive directly.
        if (status >> 10) & 3 == 1 {
            write32(register, (status & PORT_KEEP) | PORT_OWNER);
            return Err(Error::CompanionPort);
        }
        write32(register, (status & PORT_KEEP & !PORT_ENABLED) | PORT_RESET | PORT_CHANGE_BITS);
        apic::wait_ticks(6); // hold reset at least 50 ms
        write32(register, read32(register) & PORT_KEEP & !PORT_RESET);
        if !wait_until(5, || read32(register) & PORT_RESET == 0) {
            return Err(Error::NoDevice);
        }
        apic::wait_ticks(3); // reset recovery
        let after = read32(register);
        write32(register, (after & PORT_KEEP) | PORT_CHANGE_BITS);
        if after & PORT_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        if after & PORT_ENABLED == 0 {
            write32(register, (after & PORT_KEEP) | PORT_OWNER); // full speed: not ours
            return Err(Error::CompanionPort);
        }
        Ok(())
    }

    fn allocate_address(&mut self) -> Option<u8> {
        if self.next_address > 120 {
            return None;
        }
        self.next_address += 1;
        Some(self.next_address - 1)
    }

    /// The device sitting at address 0 (just reset): learn its default-pipe packet
    /// size, give it an address, read its device descriptor.
    fn address_default_device(&mut self, speed: Speed, hub_address: u8, hub_port: u8) -> Result<Found, Error> {
        // Eight bytes are enough to learn bMaxPacketSize0, and every speed's minimum
        // control packet is at least 8, so this first read is legal everywhere.
        let mut target = Target { address: 0, max_packet0: if speed == Speed::High { 64 } else { 8 }, speed, hub_address, hub_port };
        let mut first = [0u8; 8];
        self.get_descriptor(&target, 1, 0, &mut first)?;
        target.max_packet0 = u16::from(first[7]).max(8);
        let address = self.allocate_address().ok_or(Error::Unsupported)?;
        self.control_transfer(&target, 0x00, 0x05, u16::from(address), 0, &mut [])?;
        apic::wait_ticks(1); // SET_ADDRESS recovery time
        target.address = address;
        let mut descriptor = [0u8; 18];
        self.get_descriptor(&target, 1, 0, &mut descriptor)?;
        Ok(Found { target, descriptor })
    }

    /// Resets a root port and addresses whatever answers, looking through one level
    /// of hub. Fills `found` with the devices (never the hub itself); returns how many.
    pub fn enumerate_port(&mut self, port: u8, found: &mut [Option<Found>; 8]) -> Result<usize, Error> {
        self.reset_root_port(port)?;
        let device = self.address_default_device(Speed::High, 0, 0)?;
        if device.descriptor[4] != 9 {
            found[0] = Some(device);
            return Ok(1);
        }
        // A hub (on Sandy Bridge, the rate-matching hub): configure it and bring up its ports.
        let hub = device.target;
        self.set_configuration(&hub, 1)?;
        let mut hub_descriptor = [0u8; 9];
        self.control_transfer(&hub, 0xa0, 0x06, 0x2900, 0, &mut hub_descriptor)?;
        let hub_ports = hub_descriptor[2].min(8);
        let power_good_ticks = u64::from(hub_descriptor[5]).max(10) * 2 / 10 + 1; // bPwrOn2PwrGood is in 2 ms units
        let mut count = 0;
        for hub_port in 1..=hub_ports {
            match self.hub_bring_up_port(&hub, hub_port, power_good_ticks) {
                Ok(speed) => {
                    if let Ok(child) = self.address_default_device(speed, hub.address, hub_port) {
                        if count < found.len() {
                            found[count] = Some(child);
                            count += 1;
                        }
                    }
                }
                Err(_) => continue, // nothing there, or it would not reset
            }
        }
        Ok(count)
    }

    /// Powers a hub port, waits for a device, resets it, and returns the speed it
    /// came up at (hub port status: bit 9 low speed, bit 10 high speed, neither: full).
    fn hub_bring_up_port(&mut self, hub: &Target, port: u8, power_good_ticks: u64) -> Result<Speed, Error> {
        const PORT_POWER_FEATURE: u16 = 8;
        const PORT_RESET_FEATURE: u16 = 4;
        const C_PORT_CONNECTION: u16 = 16;
        const C_PORT_RESET: u16 = 20;
        let port_index = u16::from(port);
        let status = |controller: &mut Controller| -> Result<(u16, u16), Error> {
            let mut raw = [0u8; 4];
            controller.control_transfer(hub, 0xa3, 0x00, 0, port_index, &mut raw)?;
            Ok((u16::from_le_bytes([raw[0], raw[1]]), u16::from_le_bytes([raw[2], raw[3]])))
        };
        self.control_transfer(hub, 0x23, 0x03, PORT_POWER_FEATURE, port_index, &mut [])?;
        apic::wait_ticks(power_good_ticks.max(10));
        if status(self)?.0 & 1 == 0 {
            return Err(Error::NoDevice);
        }
        let _ = self.control_transfer(hub, 0x23, 0x01, C_PORT_CONNECTION, port_index, &mut []);
        self.control_transfer(hub, 0x23, 0x03, PORT_RESET_FEATURE, port_index, &mut [])?;
        for _ in 0..20 {
            apic::wait_ticks(1);
            let (state, change) = status(self)?;
            if change & (1 << 4) != 0 && state & (1 << 4) == 0 {
                let _ = self.control_transfer(hub, 0x23, 0x01, C_PORT_RESET, port_index, &mut []);
                if state & 2 == 0 {
                    return Err(Error::NoDevice);
                }
                apic::wait_ticks(3);
                return Ok(if state & (1 << 10) != 0 {
                    Speed::High
                } else if state & (1 << 9) != 0 {
                    Speed::Low
                } else {
                    Speed::Full
                });
            }
        }
        Err(Error::NoDevice)
    }

    // ---- keyboards ----

    fn keyboard_qh(&self, index: usize) -> usize {
        self.frame + OFFSET_KEYBOARD_QH + index * 0x40
    }
    fn keyboard_qtd(&self, index: usize) -> usize {
        self.frame + OFFSET_KEYBOARD_QTD + index * 0x40
    }
    fn keyboard_buffer(&self, index: usize) -> usize {
        self.frame + OFFSET_KEYBOARD_BUFFER + index * 0x20
    }

    /// Dedicates a queue head to a keyboard's interrupt-IN endpoint and arms it.
    /// Returns the keyboard's index on this controller.
    pub fn add_keyboard(&mut self, target: Target, endpoint: u8, max_packet: u16) -> Result<usize, Error> {
        let index = self.keyboards.iter().position(Option::is_none).ok_or(Error::Unsupported)?;
        self.keyboards[index] = Some(KeyboardPipe { target, endpoint, max_packet, toggle: false });
        unsafe {
            let words = self.keyboard_qh(index) as *mut u32;
            words.add(1).write_volatile(qh_characteristics(&target, endpoint, max_packet, false, false));
            words.add(2).write_volatile(qh_capabilities(&target));
        }
        self.arm_keyboard(index);
        Ok(index)
    }

    /// Queues one IN transfer of a report on keyboard `index`'s queue head.
    fn arm_keyboard(&mut self, index: usize) {
        let Some(pipe) = self.keyboards[index] else { return };
        self.set_async(false);
        let qtd = Qtd { pid: PID_IN, toggle: pipe.toggle, bytes: 8, buffer: self.keyboard_buffer(index) as u32, alternate: None };
        Self::write_qtd(self.keyboard_qtd(index) as u32, &qtd, TERMINATE, TERMINATE, true);
        unsafe {
            let words = self.keyboard_qh(index) as *mut u32;
            words.add(4).write_volatile(self.keyboard_qtd(index) as u32); // overlay: next qTD
            words.add(5).write_volatile(TERMINATE);
            for word in 6..12 {
                words.add(word).write_volatile(0); // overlay idle, so the controller loads the new qTD
            }
        }
        write32(self.operational + OP_USBSTS, 0x3f);
        self.set_async(true);
    }

    /// Checks keyboard `index`: if its report transfer finished, returns the 8-byte
    /// report and re-arms; a halted one (stall/error) is re-armed after a pause.
    pub fn poll_keyboard(&mut self, index: usize) -> Option<[u8; 8]> {
        let pipe = self.keyboards[index]?;
        let token = unsafe { ((self.keyboard_qtd(index) + 8) as *const u32).read_volatile() };
        if token & TOKEN_ACTIVE != 0 {
            return None;
        }
        let report = if token & TOKEN_HALTED == 0 {
            let report = unsafe { core::ptr::read_volatile(self.keyboard_buffer(index) as *const [u8; 8]) };
            self.keyboards[index] = Some(KeyboardPipe { toggle: !pipe.toggle, ..pipe });
            Some(report)
        } else {
            self.keyboards[index] = Some(KeyboardPipe { toggle: false, ..pipe });
            None
        };
        self.arm_keyboard(index);
        report
    }
}
