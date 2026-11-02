//! An OHCI (USB 1.1) host controller driver, built for keyboards: it takes the controller from the BIOS, resets it, powers
//! the root ports, enumerates what is plugged straight into them, runs control transfers and keeps one interrupt-IN endpoint
//! per keyboard armed. Polling only (no interrupts), root ports only (no hubs, no mass storage).
//!
//! Why OHCI matters: on a board with an EHCI controller (USB 2.0) the low- and full-speed devices, which is every keyboard,
//! are handed to the *companion* controller, UHCI (Intel, VIA) or OHCI (AMD, NVIDIA, NEC, Apple). The AMD/nForce test board
//! has OHCI companions, and until this driver its USB keyboards only worked through the BIOS's "legacy USB support", a
//! System Management Mode program that fakes a PS/2 keyboard. A kernel that takes the controller over has to supply the
//! keyboard itself.
//!
//! OHCI in one paragraph. Everything the controller does is described in memory, in two kinds of structure of 16 bytes. An
//! *endpoint descriptor* (ED) names one endpoint (device address, endpoint number, speed, maximum packet size) and holds two
//! pointers into a queue of *transfer descriptors* (TD): `HeadP`, the next TD to run, and `TailP`, a dummy TD that is never
//! run. The queue is empty when the two are equal, so software appends work by filling in the dummy and pointing `TailP` at a
//! fresh one; the controller never sees a half-built TD. A TD is one buffer and a direction: its first word carries the PID
//! (SETUP/OUT/IN), the data toggle, and, written back by the controller, a condition code (0 = done without error, 0xF = not
//! yet touched). EDs hang on lists: the *control* list (walked every frame), the *bulk* list (unused here) and the
//! *periodic* list, reached through the 32 slots of the HCCA (the host controller communications area, 256 bytes the
//! controller reads and writes): frame N walks the chain in slot N mod 32. A keyboard's ED sits in every 8th slot, so it is
//! polled every 8 ms.
//!
//! The firmware handshake. If the BIOS drives the controller through SMM, `HcControl.InterruptRouting` is set: write
//! `OwnershipChangeRequest` and the SMM code lets go (clears it). Controllers that implement the legacy keyboard emulation
//! have extra registers at 0x100 (`HceControl`); clearing them stops the PS/2 impersonation.

use crate::{memory, paging, pci, timer};
use core::sync::atomic::{Ordering, fence};

pub const MAX_CONTROLLERS: usize = 2;
pub const MAX_KEYBOARDS: usize = 2;
const TIMEOUT_TICKS: u64 = 100;

// Operational registers (offsets from the BAR).
const HC_REVISION: usize = 0x00;
const HC_CONTROL: usize = 0x04;
const HC_COMMAND_STATUS: usize = 0x08;
const HC_INTERRUPT_STATUS: usize = 0x0c;
const HC_INTERRUPT_DISABLE: usize = 0x14;
const HC_HCCA: usize = 0x18;
const HC_CONTROL_HEAD_ED: usize = 0x20;
const HC_BULK_HEAD_ED: usize = 0x28;
const HC_FM_INTERVAL: usize = 0x34;
const HC_PERIODIC_START: usize = 0x40;
const HC_LS_THRESHOLD: usize = 0x44;
const HC_RH_DESCRIPTOR_A: usize = 0x48;
const HC_RH_STATUS: usize = 0x50;
const HC_RH_PORT_STATUS: usize = 0x54;
/// The legacy keyboard/mouse emulation control register, present when HcRevision bit 8 is set.
const HCE_CONTROL: usize = 0x100;

const CONTROL_PERIODIC_ENABLE: u32 = 1 << 2;
const CONTROL_CONTROL_ENABLE: u32 = 1 << 4;
#[allow(dead_code)]
const CONTROL_FUNCTIONAL_STATE: u32 = 3 << 6;
const CONTROL_OPERATIONAL: u32 = 2 << 6;
const CONTROL_INTERRUPT_ROUTING: u32 = 1 << 8;
const CONTROL_RATIO_4_TO_1: u32 = 3;

const COMMAND_RESET: u32 = 1 << 0;
const COMMAND_CONTROL_LIST_FILLED: u32 = 1 << 1;
const COMMAND_OWNERSHIP_REQUEST: u32 = 1 << 3;

const RH_LOCAL_POWER_SET: u32 = 1 << 16; // HcRhStatus: write 1 = SetGlobalPower

// HcRhPortStatus bits. Reads: status; writes: 1 in a "set/clear" bit does it, 1 in a change bit clears the change.
const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 1;
const PORT_SET_ENABLE: u32 = 1 << 1;
const PORT_SET_RESET: u32 = 1 << 4;
const PORT_SET_POWER: u32 = 1 << 8;
const PORT_LOW_SPEED: u32 = 1 << 9;
const PORT_CONNECT_CHANGE: u32 = 1 << 16;
const PORT_RESET_CHANGE: u32 = 1 << 20;

// One page of controller structures (HCCA first: it must be 256-byte aligned), one page of data buffers.
const OFFSET_CONTROL_ED: usize = 0x100;
const OFFSET_KEYBOARD_ED: usize = 0x110; // + 0x10 per keyboard
const OFFSET_CONTROL_TDS: usize = 0x200;
const CONTROL_TD_COUNT: usize = 16;
const OFFSET_KEYBOARD_TDS: usize = 0x300; // + 0x20 per keyboard: two TDs, used in turn
const BUFFER_SETUP: usize = 0x00;
const BUFFER_DATA: usize = 0x100;
const BUFFER_DATA_BYTES: usize = 1024;
const BUFFER_KEYBOARD: usize = 0x800; // + 0x10 per keyboard

// TD control word.
const TD_ROUNDING: u32 = 1 << 18; // a short packet is not an error
const TD_SETUP: u32 = 0;
const TD_OUT: u32 = 1 << 19;
const TD_IN: u32 = 2 << 19;
const TD_NO_INTERRUPT: u32 = 7 << 21;
const TD_TOGGLE_FROM_TD: u32 = 2 << 24;
const TD_DATA0: u32 = TD_TOGGLE_FROM_TD;
const TD_DATA1: u32 = TD_TOGGLE_FROM_TD | (1 << 24);
const TD_NOT_ACCESSED: u32 = 0xf << 28;
const CC_NO_ERROR: u32 = 0;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // payloads are shown through Debug
pub enum Error {
    NoDevice,
    Unsupported,
    NoMemory,
    Timeout,
    /// The device answered with a condition code (4 = stall, 5 = no response, 3 = toggle mismatch ...).
    Transfer(u8),
}

/// What a device needs to be addressed: its address, the maximum packet size of its default pipe, and whether it is low speed.
#[derive(Clone, Copy)]
pub struct Target {
    pub address: u8,
    pub max_packet0: u16,
    pub low_speed: bool,
}

/// A device found by enumeration.
#[derive(Clone, Copy)]
pub struct Found {
    pub target: Target,
    pub descriptor: [u8; 18],
}

#[derive(Clone, Copy)]
#[allow(dead_code)] // target and endpoint describe the pipe
struct KeyboardPipe {
    target: Target,
    endpoint: u8,
    max_packet: u16,
    /// Which of the keyboard's two TDs is armed (the other is the dummy at the tail).
    armed: usize,
    root_port: u8,
}

pub struct Controller {
    pub pci: pci::Device,
    mmio: usize,
    pub ports: u8,
    frame: usize,   // the structures page (HCCA, EDs, TDs)
    buffers: usize, // the data buffers page
    next_address: u8,
    /// The TD the control ED's dummy tail is (an index into the ring of `CONTROL_TD_COUNT`).
    control_tail: usize,
    /// Root ports where a keyboard was found (bit n = port n); rescans skip them.
    pub claimed_ports: u16,
    keyboards: [Option<KeyboardPipe>; MAX_KEYBOARDS],
    /// What enumeration was doing when it last failed (for the boot log).
    pub step: &'static str,
}

static mut CONTROLLERS: [Option<Controller>; MAX_CONTROLLERS] = [const { None }; MAX_CONTROLLERS];
static mut COUNT: usize = 0;

pub fn count() -> usize {
    unsafe { COUNT }
}

pub fn controller(index: usize) -> Option<&'static mut Controller> {
    unsafe { (*core::ptr::addr_of_mut!(CONTROLLERS)).get_mut(index)?.as_mut() }
}

fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}

fn write32(address: usize, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) }
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = timer::ticks();
    loop {
        crate::debug::pet();
        if condition() {
            return true;
        }
        if timer::ticks().wrapping_sub(start) >= ticks {
            return condition();
        }
        core::hint::spin_loop();
    }
}

/// Starts every OHCI controller (PCI class 0x0c, subclass 0x03, interface 0x10).
pub fn init_all() -> usize {
    for device in pci::devices().filter(|d| d.class == 0x0c && d.subclass == 0x03 && d.interface == 0x10) {
        if count() == MAX_CONTROLLERS {
            break;
        }
        match Controller::init(device) {
            Ok(controller) => unsafe {
                CONTROLLERS[COUNT] = Some(controller);
                COUNT += 1;
            },
            Err(error) => crate::println!("OHCI at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    count()
}

impl Controller {
    fn init(device: pci::Device) -> Result<Controller, Error> {
        device.force_power_state_d0();
        device.enable_memory_and_bus_master();
        let (base, size) = device.memory_bar(0).ok_or(Error::NoDevice)?;
        paging::map_device(base, size.max(0x1000));
        let mmio = base as usize;
        let revision = read32(mmio + HC_REVISION);
        if revision & 0xff != 0x10 && revision & 0xff != 0x11 {
            return Err(Error::Unsupported);
        }
        // The BIOS's PS/2 impersonation of USB keyboards (where the controller has the emulation registers): off.
        if revision & 0x100 != 0 {
            write32(mmio + HCE_CONTROL, 0);
        }
        // Take the controller from firmware that drives it through SMM: ask, and wait for it to let go.
        if read32(mmio + HC_CONTROL) & CONTROL_INTERRUPT_ROUTING != 0 {
            write32(mmio + HC_INTERRUPT_DISABLE, 0x8000_0000);
            write32(mmio + HC_COMMAND_STATUS, COMMAND_OWNERSHIP_REQUEST);
            if !wait_until(100, || read32(mmio + HC_CONTROL) & CONTROL_INTERRUPT_ROUTING == 0) {
                return Err(Error::Timeout);
            }
        }
        // No interrupts of ours: everything is polled. Then a functional reset (which leaves the controller suspended;
        // it must be operational again within 2 ms, so the structures are built first).
        write32(mmio + HC_INTERRUPT_DISABLE, 0xc000_007f);
        write32(mmio + HC_INTERRUPT_STATUS, 0x7f);

        // The structures: one page (HCCA at its start, then the EDs and TDs) and one page of buffers, below 4 GiB because the
        // controller's pointers are 32 bits.
        let frame = memory::allocate_contiguous(1, Some(1 << 32)).ok_or(Error::NoMemory)? as usize;
        let buffers = memory::allocate_contiguous(1, Some(1 << 32)).ok_or(Error::NoMemory)? as usize;
        unsafe {
            core::ptr::write_bytes(frame as *mut u8, 0, 4096);
            core::ptr::write_bytes(buffers as *mut u8, 0, 4096);
        }
        // The control ED starts empty (HeadP == TailP == the first ring TD) and skipped until the first transfer.
        let tail = frame + OFFSET_CONTROL_TDS;
        let control_ed = (frame + OFFSET_CONTROL_ED) as *mut u32;
        unsafe {
            control_ed.write_volatile(1 << 14);
            control_ed.add(1).write_volatile(tail as u32);
            control_ed.add(2).write_volatile(tail as u32);
            control_ed.add(3).write_volatile(0);
        }

        let interval = read32(mmio + HC_FM_INTERVAL);
        write32(mmio + HC_COMMAND_STATUS, COMMAND_RESET);
        if !wait_until(10, || read32(mmio + HC_COMMAND_STATUS) & COMMAND_RESET == 0) {
            return Err(Error::Timeout);
        }
        // Frame timing as the specification gives it: 11999 bit times per frame (FI 0x2edf), the largest packet that fits
        // (FSLargestDataPacket), the toggle bit flipped so the controller notices the new value.
        let frame_interval = if interval & 0x3fff == 0 { 0x2edf } else { interval & 0x3fff };
        write32(mmio + HC_HCCA, frame as u32);
        write32(mmio + HC_CONTROL_HEAD_ED, (frame + OFFSET_CONTROL_ED) as u32);
        write32(mmio + HC_BULK_HEAD_ED, 0);
        write32(mmio + HC_FM_INTERVAL, frame_interval | ((((frame_interval - 210) * 6) / 7) << 16) | ((interval ^ 0x8000_0000) & 0x8000_0000));
        write32(mmio + HC_PERIODIC_START, frame_interval * 9 / 10);
        write32(mmio + HC_LS_THRESHOLD, 0x628);
        write32(mmio + HC_CONTROL, CONTROL_CONTROL_ENABLE | CONTROL_PERIODIC_ENABLE | CONTROL_OPERATIONAL | CONTROL_RATIO_4_TO_1);

        // Power the root ports (globally, and one by one where the hub switches them individually) and give devices time to
        // announce themselves (the hub descriptor says how long the supply needs to settle, in 2 ms units).
        let descriptor_a = read32(mmio + HC_RH_DESCRIPTOR_A);
        let ports = (descriptor_a & 0xff) as u8;
        if ports == 0 || ports > 15 {
            return Err(Error::Unsupported);
        }
        write32(mmio + HC_RH_STATUS, RH_LOCAL_POWER_SET);
        for port in 1..=ports {
            write32(mmio + HC_RH_PORT_STATUS + 4 * (usize::from(port) - 1), PORT_SET_POWER);
        }
        timer::wait_ticks(u64::from(descriptor_a >> 24) / 5 + 10);
        Ok(Controller { pci: device, mmio, ports, frame, buffers, next_address: 1, control_tail: 0, claimed_ports: 0, keyboards: [None; MAX_KEYBOARDS], step: "" })
    }

    fn port_register(&self, port: u8) -> usize {
        self.mmio + HC_RH_PORT_STATUS + 4 * (usize::from(port) - 1)
    }

    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    pub fn port_has_device(&self, port: u8) -> bool {
        self.port_status(port) & PORT_CONNECTED != 0
    }

    /// The connection-status-change bit: something was plugged or unplugged since it was last cleared.
    pub fn port_connect_changed(&self, port: u8) -> bool {
        self.port_status(port) & PORT_CONNECT_CHANGE != 0
    }

    // ---- control transfers ----

    fn control_td(&self, index: usize) -> *mut u32 {
        (self.frame + OFFSET_CONTROL_TDS + 16 * (index % CONTROL_TD_COUNT)) as *mut u32
    }

    fn fill_td(td: *mut u32, control: u32, buffer: usize, length: usize, next: usize) {
        unsafe {
            td.write_volatile(control | TD_NOT_ACCESSED);
            td.add(1).write_volatile(if length == 0 { 0 } else { buffer as u32 });
            td.add(2).write_volatile(next as u32);
            td.add(3).write_volatile(if length == 0 { 0 } else { (buffer + length - 1) as u32 });
        }
    }

    /// One control transfer on the default pipe of `target`: SETUP, an optional data stage (at most `BUFFER_DATA_BYTES`), and
    /// the status stage in the opposite direction. Returns the bytes moved in the data stage.
    pub fn control_transfer(&mut self, target: &Target, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8]) -> Result<usize, Error> {
        if data.len() > BUFFER_DATA_BYTES {
            return Err(Error::Unsupported);
        }
        let data_in = request_type & 0x80 != 0;
        let setup = self.buffers + BUFFER_SETUP;
        let packet = [request_type, request, value as u8, (value >> 8) as u8, index as u8, (index >> 8) as u8, data.len() as u8, (data.len() >> 8) as u8];
        unsafe { core::ptr::copy_nonoverlapping(packet.as_ptr(), setup as *mut u8, 8) };
        let buffer = self.buffers + BUFFER_DATA;
        if !data_in && !data.is_empty() {
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buffer as *mut u8, data.len()) };
        }

        // Fill the TDs starting at the dummy tail of the ring, then a fresh dummy after them.
        let has_data = !data.is_empty();
        let count = if has_data { 3 } else { 2 };
        let first = self.control_tail;
        let dummy = first + count;
        let ring = self.frame + OFFSET_CONTROL_TDS;
        let td_address = move |position: usize| ring + 16 * (position % CONTROL_TD_COUNT);
        Self::fill_td(self.control_td(first), TD_SETUP | TD_NO_INTERRUPT | TD_DATA0, setup, 8, td_address(first + 1));
        if has_data {
            let direction = if data_in { TD_IN } else { TD_OUT };
            Self::fill_td(self.control_td(first + 1), direction | TD_ROUNDING | TD_NO_INTERRUPT | TD_DATA1, buffer, data.len(), td_address(first + 2));
        }
        let status_direction = if has_data && data_in { TD_OUT } else { TD_IN };
        Self::fill_td(self.control_td(first + count - 1), status_direction | TD_NO_INTERRUPT | TD_DATA1, 0, 0, td_address(dummy));
        unsafe { self.control_td(dummy).write_bytes(0, 4) };

        // The ED: this device's address, speed and packet size (skip bit set while it changes), then the new tail.
        let ed = (self.frame + OFFSET_CONTROL_ED) as *mut u32;
        let flags = u32::from(target.address) | (u32::from(target.low_speed) << 13) | (u32::from(target.max_packet0) << 16);
        unsafe {
            ed.write_volatile(flags | (1 << 14));
            ed.add(2).write_volatile(td_address(first) as u32); // HeadP: the first TD (clears Halted and the toggle carry)
            ed.add(1).write_volatile(td_address(dummy) as u32); // TailP: the new dummy
            fence(Ordering::SeqCst);
            ed.write_volatile(flags);
        }
        self.control_tail = dummy % CONTROL_TD_COUNT;
        write32(self.mmio + HC_COMMAND_STATUS, COMMAND_CONTROL_LIST_FILLED);

        // Done when the status TD has been touched (or an earlier one failed).
        let last = self.control_td(first + count - 1);
        let finished = wait_until(TIMEOUT_TICKS, || {
            let status_done = unsafe { last.read_volatile() } >> 28 != 0xf;
            let failed = (0..count).any(|n| {
                let code = unsafe { self.control_td(first + n).read_volatile() } >> 28;
                code != 0xf && code != CC_NO_ERROR
            });
            status_done || failed
        });
        let mut result = if finished { Ok(()) } else { Err(Error::Timeout) };
        if finished {
            for n in 0..count {
                let code = unsafe { self.control_td(first + n).read_volatile() } >> 28;
                if code != CC_NO_ERROR {
                    result = Err(Error::Transfer(code as u8));
                    break;
                }
            }
        }
        if result.is_err() {
            // An error halts the ED: put HeadP on the tail again (clearing the halt) and skip what is left.
            unsafe {
                ed.write_volatile(flags | (1 << 14));
                ed.add(2).write_volatile(td_address(dummy) as u32);
            }
            self.control_tail = dummy % CONTROL_TD_COUNT;
        }
        result?;
        let moved = if has_data && data_in {
            // CurrentBufferPointer is 0 when the whole buffer was filled, or points at the first byte not written.
            let cbp = unsafe { self.control_td(first + 1).add(1).read_volatile() } as usize;
            let moved = if cbp == 0 { data.len() } else { cbp - buffer };
            unsafe { core::ptr::copy_nonoverlapping(buffer as *const u8, data.as_mut_ptr(), moved.min(data.len())) };
            moved
        } else {
            data.len()
        };
        Ok(moved)
    }

    pub fn get_descriptor(&mut self, target: &Target, kind: u8, index: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(target, 0x80, 0x06, (u16::from(kind) << 8) | u16::from(index), 0, buffer)
    }

    pub fn set_configuration(&mut self, target: &Target, value: u8) -> Result<(), Error> {
        self.control_transfer(target, 0x00, 0x09, u16::from(value), 0, &mut []).map(|_| ())
    }

    // ---- root ports and enumeration ----

    /// Resets a root port (10 ms or more of reset signalling) and reports whether it came up enabled.
    fn reset_port(&mut self, port: u8) -> bool {
        let register = self.port_register(port);
        write32(register, PORT_SET_RESET);
        if !wait_until(30, || read32(register) & PORT_RESET_CHANGE != 0) {
            return false;
        }
        write32(register, PORT_RESET_CHANGE | PORT_CONNECT_CHANGE);
        timer::wait_ticks(3); // recovery time before the first request
        if read32(register) & PORT_ENABLED == 0 {
            write32(register, PORT_SET_ENABLE);
            timer::wait_ticks(1);
        }
        read32(register) & PORT_ENABLED != 0
    }

    /// Resets a root port, addresses what answers and reads its device descriptor.
    pub fn enumerate_port(&mut self, port: u8) -> Result<Found, Error> {
        self.step = "look at the root port";
        let status = self.port_status(port);
        if status & PORT_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        self.step = "reset the root port";
        if !self.reset_port(port) {
            return Err(Error::NoDevice);
        }
        let low_speed = self.port_status(port) & PORT_LOW_SPEED != 0;
        let mut target = Target { address: 0, max_packet0: 8, low_speed };
        self.step = "read the first 8 bytes of the device descriptor at address 0";
        let mut first = [0u8; 8];
        let mut tries = 0;
        loop {
            match self.get_descriptor(&target, 1, 0, &mut first) {
                Ok(_) => break,
                Err(error) if tries >= 3 => return Err(error),
                Err(_) => {
                    tries += 1;
                    timer::wait_ticks(5);
                }
            }
        }
        target.max_packet0 = u16::from(first[7]).max(8);
        let address = self.next_address;
        self.next_address = if self.next_address >= 126 { 1 } else { self.next_address + 1 };
        self.step = "set the device address";
        self.control_transfer(&target, 0x00, 0x05, u16::from(address), 0, &mut [])?;
        timer::wait_ticks(2); // the device may take 2 ms to start answering at its new address
        target.address = address;
        self.step = "read the device descriptor";
        let mut descriptor = [0u8; 18];
        self.get_descriptor(&target, 1, 0, &mut descriptor)?;
        Ok(Found { target, descriptor })
    }

    // ---- keyboards ----

    fn keyboard_ed(&self, index: usize) -> *mut u32 {
        (self.frame + OFFSET_KEYBOARD_ED + 0x10 * index) as *mut u32
    }

    fn keyboard_td(&self, index: usize, which: usize) -> *mut u32 {
        (self.frame + OFFSET_KEYBOARD_TDS + 0x20 * index + 0x10 * (which & 1)) as *mut u32
    }

    fn keyboard_buffer(&self, index: usize) -> usize {
        self.buffers + BUFFER_KEYBOARD + 0x10 * index
    }

    /// Hangs the active keyboard EDs on the periodic list: every 8th of the 32 HCCA slots points at the chain. The periodic
    /// list is switched off while the links change.
    fn relink(&mut self) {
        let control = read32(self.mmio + HC_CONTROL);
        write32(self.mmio + HC_CONTROL, control & !CONTROL_PERIODIC_ENABLE);
        timer::wait_ticks(2);
        let mut previous: Option<*mut u32> = None;
        let mut first = 0u32;
        for index in 0..MAX_KEYBOARDS {
            if self.keyboards[index].is_some() {
                let ed = self.keyboard_ed(index);
                unsafe { ed.add(3).write_volatile(0) };
                match previous {
                    Some(before) => unsafe { before.add(3).write_volatile(ed as u32) },
                    None => first = ed as u32,
                }
                previous = Some(ed);
            }
        }
        for slot in (0..32).step_by(8) {
            unsafe { (self.frame as *mut u32).add(slot).write_volatile(first) };
        }
        write32(self.mmio + HC_CONTROL, control | CONTROL_PERIODIC_ENABLE);
    }

    /// Queues one IN transfer of a report on keyboard `index`: the TD at the tail is filled in and a new dummy follows it.
    fn arm_keyboard(&mut self, index: usize, which: usize) {
        let Some(pipe) = self.keyboards[index] else { return };
        let td = self.keyboard_td(index, which);
        let dummy = self.keyboard_td(index, which + 1);
        unsafe { dummy.write_bytes(0, 4) };
        // Toggle from the ED's carry bit (T = 0): the controller keeps DATA0/DATA1 across the keyboard's reports.
        Self::fill_td(td, TD_IN | TD_ROUNDING | TD_NO_INTERRUPT, self.keyboard_buffer(index), usize::from(pipe.max_packet.min(8)), dummy as usize);
        let ed = self.keyboard_ed(index);
        fence(Ordering::SeqCst);
        unsafe { ed.add(1).write_volatile(dummy as u32) };
        if let Some(entry) = self.keyboards[index].as_mut() {
            entry.armed = which;
        }
    }

    /// Dedicates an endpoint descriptor to a keyboard's interrupt-IN endpoint and arms it. Returns its index.
    pub fn add_keyboard(&mut self, target: Target, endpoint: u8, max_packet: u16, root_port: u8) -> Result<usize, Error> {
        let index = self.keyboards.iter().position(Option::is_none).ok_or(Error::Unsupported)?;
        let ed = self.keyboard_ed(index);
        let td = self.keyboard_td(index, 0);
        let flags = u32::from(target.address) | (u32::from(endpoint & 0xf) << 7) | (u32::from(target.low_speed) << 13) | (u32::from(max_packet.min(8)) << 16);
        unsafe {
            ed.write_volatile(flags | (1 << 14)); // skipped until it is armed and linked
            ed.add(1).write_volatile(td as u32);
            ed.add(2).write_volatile(td as u32);
            ed.add(3).write_volatile(0);
        }
        self.keyboards[index] = Some(KeyboardPipe { target, endpoint, max_packet, armed: 0, root_port });
        self.arm_keyboard(index, 0);
        unsafe { ed.write_volatile(flags) };
        self.relink();
        Ok(index)
    }

    /// Takes the keyboards on root `port` off the periodic list (the device went away or came back). Returns a bit mask of
    /// the removed keyboard indexes.
    pub fn remove_keyboards_on_port(&mut self, port: u8) -> u8 {
        let mut removed = 0u8;
        for index in 0..MAX_KEYBOARDS {
            if self.keyboards[index].is_some_and(|pipe| pipe.root_port == port) {
                unsafe { self.keyboard_ed(index).write_volatile(1 << 14) }; // skip it
                self.keyboards[index] = None;
                removed |= 1 << index;
            }
        }
        if removed != 0 {
            self.relink();
        }
        removed
    }

    /// Checks keyboard `index`: if its report transfer finished, returns the 8-byte report and re-arms; a failed one
    /// (stall, no answer) is cleared and re-armed.
    pub fn poll_keyboard(&mut self, index: usize) -> Option<[u8; 8]> {
        let pipe = self.keyboards[index]?;
        let td = self.keyboard_td(index, pipe.armed);
        let code = unsafe { td.read_volatile() } >> 28;
        if code == 0xf {
            return None;
        }
        let report = if code == CC_NO_ERROR {
            Some(unsafe { core::ptr::read_volatile(self.keyboard_buffer(index) as *const [u8; 8]) })
        } else {
            // An error halts the ED (bit 0 of HeadP): clear the halt, the failed TD is already retired.
            let head = unsafe { self.keyboard_ed(index).add(2).read_volatile() };
            unsafe { self.keyboard_ed(index).add(2).write_volatile(head & !1) };
            None
        };
        self.arm_keyboard(index, pipe.armed + 1);
        report
    }

    /// The root-hub status registers' view, for `usb`: acknowledges nothing.
    pub fn describe_port(&self, port: u8) -> Option<(bool, bool)> {
        let status = self.port_status(port);
        (status & PORT_CONNECTED != 0).then_some((status & PORT_LOW_SPEED != 0, self.claimed_ports & (1 << port) != 0))
    }
}
