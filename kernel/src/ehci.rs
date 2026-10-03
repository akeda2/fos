//! A minimal EHCI (USB 2.0 high-speed) host controller driver, enough to read
//! and write a USB stick: device enumeration on the root ports (and one level of
//! hub behind them), control transfers, and bulk transfers.
//!
//! Why it exists: both older test machines boot from a stick on EHCI ports (the
//! Sandy Bridge's chipset has no other USB 2 controller; the AMD/nForce board has
//! EHCI plus an OHCI companion). While the BIOS is in charge it emulates the stick
//! through INT13h; the moment fos takes the controller over that is gone, so only
//! a native driver keeps the boot disk usable -- the same story as `uhci.rs` and
//! `xhci.rs`. Sandy Bridge detail: Intel's 6-series EHCI routes every port
//! through an integrated "rate matching hub", which software sees as an ordinary
//! USB hub on the root port, so `enumerate_hub` is not optional there.
//!
//! EHCI in one paragraph. The controller walks *schedules* of descriptors in RAM.
//! Control and bulk traffic uses the *asynchronous* schedule: a circular list of
//! Queue Heads (QHs). A QH names one endpoint (device address, endpoint number,
//! speed, max packet size) and holds an *overlay* -- a working copy of the qTD it
//! is executing -- plus a pointer to the next queue transfer descriptor (qTD).
//! A qTD is one chunk of a transfer: a PID (SETUP/IN/OUT), a byte count, the data
//! toggle, up to five 4 KiB buffer page pointers and a status byte the controller
//! rewrites (Active clears when it is done, Halted and error bits say why not).
//! The periodic schedule (interrupt/isochronous) is not used.
//!
//! This driver keeps the asynchronous list to ONE QH and reprograms it for every
//! transfer, with the schedule switched off while it does: simple and safe, at the
//! price of about a microframe of latency per transfer, which a disk does not
//! notice. Polling only; no interrupts. Full- and low-speed devices are handed to
//! the companion controller (there is no OHCI/UHCI driver for those on EHCI
//! machines) -- fos only ever wants sticks, which are high-speed.

use core::cell::UnsafeCell;

use crate::interrupts;
use crate::paging;
use crate::pci::{self, PciAddress};
use crate::pmem;
use crate::uhci;

pub const MAX_INSTANCES: usize = 2;
const MAX_QTDS: usize = 16;
/// One qTD carries at most this much (five pages, with the buffer start anywhere
/// in the first one, always covers 16 KiB).
const QTD_MAX_BYTES: usize = 16 * 1024;
/// Largest bulk transfer in one call: four qTDs.
pub const MAX_BULK_BYTES: usize = 4 * QTD_MAX_BYTES;
/// Largest control data stage, bounced through the controller's own DMA frame.
const MAX_CONTROL_BYTES: usize = 1024;
const TIMEOUT_TICKS: u64 = 100;
const BULK_TIMEOUT_TICKS: u64 = 500;

// ---- capability registers ----
const CAP_HCSPARAMS: u32 = 0x04;
const CAP_HCCPARAMS: u32 = 0x08;

// ---- operational registers (from BAR + CAPLENGTH) ----
const OP_USBCMD: u32 = 0x00;
const OP_USBSTS: u32 = 0x04;
const OP_USBINTR: u32 = 0x08;
const OP_CTRLDSSEGMENT: u32 = 0x10;
const OP_ASYNCLISTADDR: u32 = 0x18;
const OP_CONFIGFLAG: u32 = 0x40;
const OP_PORTSC: u32 = 0x44;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_ASYNC_ENABLE: u32 = 1 << 5;
const STS_HALTED: u32 = 1 << 12;
const STS_ASYNC_RUNNING: u32 = 1 << 15;

// ---- PORTSC bits ----
const PORT_CONNECTED: u32 = 1 << 0;
const PORT_ENABLED: u32 = 1 << 2;
const PORT_RESET: u32 = 1 << 8;
const PORT_LINE_STATUS_SHIFT: u32 = 10;
const PORT_POWER: u32 = 1 << 12;
const PORT_OWNER: u32 = 1 << 13;
/// Write-1-to-clear change bits: connect change, enable change, over-current change.
const PORT_CHANGE_BITS: u32 = (1 << 1) | (1 << 3) | (1 << 5);
/// What a read-modify-write of PORTSC keeps: enabled (writing 0 would disable
/// the port), power, owner, indicator, test control and wake bits.
const PORT_KEEP: u32 = PORT_ENABLED | PORT_POWER | PORT_OWNER | (0x3 << 14) | (0xf << 16) | (0x7 << 20);

// ---- qTD token bits ----
const TOKEN_ACTIVE: u32 = 1 << 7;
const TOKEN_HALTED: u32 = 1 << 6;
const TOKEN_ERRORS: u32 = 0x3e; // buffer error, babble, transaction error, missed microframe
const PID_OUT: u32 = 0;
const PID_IN: u32 = 1;
const PID_SETUP: u32 = 2;
const LIST_TERMINATE: u32 = 1;

// ---- layout of the controller's one DMA frame ----
const OFFSET_QH: u32 = 0x000;
const OFFSET_QTDS: u32 = 0x100;
const QTD_STRIDE: u32 = 0x40;
const OFFSET_SETUP: u32 = 0xb00;
const OFFSET_BOUNCE: u32 = 0xc00;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NotPresent,
    InitFailed,
    Timeout,
    NoDevice,
    /// The device is full- or low-speed; the port was released to the companion
    /// controller.
    NotHighSpeed,
    /// A transfer halted; the value is the qTD status byte (0x40 alone is STALL,
    /// the other bits say transaction error, babble, ...).
    Transfer(u8),
    NoAddress,
    Unsupported,
}

impl Error {
    /// The error in `usb_storage`'s vocabulary: a timeout stays one, anything else
    /// carries the qTD status in the "failed TD control word" field, tagged with
    /// bit 25 to tell it from a UHCI word (bit 24 marks xHCI completion codes).
    pub fn as_uhci(self) -> uhci::Error {
        match self {
            Error::Timeout => uhci::Error::Timeout,
            Error::Transfer(status) => uhci::Error::TransferFailed(0x0200_0000 | u32::from(status)),
            _ => uhci::Error::TransferFailed(0x0200_0000),
        }
    }
}

fn read32(address: u32) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn write32(address: u32, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

fn wait_ticks(count: u64) {
    let start = interrupts::ticks();
    while interrupts::ticks().wrapping_sub(start) < count {
        // Waiting is progress as far as the watchdog is concerned: a whole USB
        // discovery (many ports, retries) can take longer than its limit.
        crate::debug::watchdog_pet();
    }
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = interrupts::ticks();
    loop {
        crate::debug::watchdog_pet();
        if condition() {
            return true;
        }
        if interrupts::ticks().wrapping_sub(start) >= ticks {
            return condition();
        }
    }
}

/// A device found by `Ehci::enumerate_port`: addressed, with its device
/// descriptor read.
#[derive(Clone, Copy)]
pub struct EnumeratedDevice {
    pub root_port: u8,
    /// Port number on the hub above it, 0 for a device on the root port itself.
    pub hub_port: u8,
    pub address: u8,
    pub max_packet0: u16,
    pub descriptor: [u8; 18],
}

/// One qTD to build: its direction, data toggle, size, buffer and whether it
/// interrupts (unused here, but the bit documents the last one).
#[derive(Clone, Copy)]
struct Qtd {
    pid: u32,
    toggle: bool,
    bytes: u32,
    buffer: u32,
    /// Index of the qTD a short packet should continue at (the status stage of a
    /// control read), instead of ending the chain.
    alternate: Option<usize>,
}

struct State {
    /// Bit n set: device address n is in use on this controller's bus.
    addresses: u128,
    /// Per root port: the addresses this driver handed out there (the device or
    /// hub first, then whatever sits behind a hub), so reopening the port frees them.
    port_addresses: [[u8; 8]; 16],
}

pub struct Ehci {
    instance: usize,
    mmio: u32,
    operational: u32,
    ports: u8,
    /// The frame holding the QH, the qTDs, the setup packet and a control bounce
    /// buffer (see the `OFFSET_*` constants).
    frame: u32,
    state: UnsafeCell<State>,
}

static mut CONTROLLERS: [Option<Ehci>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CONTROLLER_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

/// Starts every EHCI controller. **Taking a controller over ends the BIOS's own
/// INT13h emulation of any USB disk behind it**, so nothing calls this at boot.
pub fn initialize_all() -> Result<usize, Error> {
    unsafe {
        if INITIALIZED {
            return if CONTROLLER_COUNT == 0 { Err(Error::NotPresent) } else { Ok(CONTROLLER_COUNT) };
        }
        INITIALIZED = true;
    }
    let mut matches: [Option<PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_ehci_controllers(&mut matches);
    if found == 0 {
        return Err(Error::NotPresent);
    }
    let mut failed = false;
    for address in matches[..found.min(MAX_INSTANCES)].iter().flatten() {
        if init_controller(*address).is_err() {
            failed = true;
        }
    }
    unsafe {
        if CONTROLLER_COUNT == 0 {
            Err(if failed { Error::InitFailed } else { Error::NotPresent })
        } else {
            Ok(CONTROLLER_COUNT)
        }
    }
}

pub fn count() -> usize {
    initialize_all().unwrap_or(0)
}

pub fn get(index: usize) -> Option<&'static Ehci> {
    initialize_all().ok()?;
    unsafe {
        if index >= CONTROLLER_COUNT {
            return None;
        }
        (*core::ptr::addr_of!(CONTROLLERS))[index].as_ref()
    }
}

/// The BIOS handshake in PCI configuration space (EHCI keeps its "USB Legacy
/// Support" extended capability there, at the offset HCCPARAMS.EECP gives):
/// set the OS-owned semaphore, wait for the BIOS-owned one to clear, then turn
/// off the firmware's SMI sources.
fn take_ownership_from_bios(address: PciAddress, hccparams: u32) {
    let offset = ((hccparams >> 8) & 0xff) as u8;
    if offset < 0x40 {
        return;
    }
    let read = |at: u8| pci::config_read32(address.bus(), address.device(), address.function(), at);
    let write = |at: u8, value: u32| pci::config_write32(address.bus(), address.device(), address.function(), at, value);
    let header = read(offset);
    if header & 0xff != 1 {
        return;
    }
    const BIOS_OWNED: u32 = 1 << 16;
    const OS_OWNED: u32 = 1 << 24;
    if header & BIOS_OWNED != 0 {
        write(offset, header | OS_OWNED);
        wait_until(100, || read(offset) & BIOS_OWNED == 0);
    }
    // USBLEGCTLSTS: SMI enables off, status bits (write 1 to clear) acknowledged.
    write(offset + 4, 0xe000_0000);
}

fn init_controller(address: PciAddress) -> Result<(), Error> {
    let instance = unsafe { CONTROLLER_COUNT };
    if instance >= MAX_INSTANCES {
        return Err(Error::InitFailed);
    }
    crate::debug::crumb(0x1001);
    address.force_power_state_d0();
    address.enable_memory_and_bus_master();
    let (base, size) = address.memory_bar_region(0).ok_or(Error::InitFailed)?;
    paging::map_mmio(base, size).map_err(|_| Error::InitFailed)?;
    crate::debug::crumb(0x1002);

    let mmio = base;
    let cap_length = u32::from(unsafe { core::ptr::read_volatile(mmio as *const u8) });
    let hcsparams = read32(mmio + CAP_HCSPARAMS);
    let hccparams = read32(mmio + CAP_HCCPARAMS);
    let operational = mmio + cap_length;
    let ports = (hcsparams & 0xf) as u8;
    if ports == 0 || ports > 15 {
        return Err(Error::InitFailed);
    }

    take_ownership_from_bios(address, hccparams);
    crate::debug::crumb(0x1003);

    // Stop, then reset; HCRESET clears every register to its default.
    let command = read32(operational + OP_USBCMD);
    if command & CMD_RUN != 0 {
        write32(operational + OP_USBCMD, command & !(CMD_RUN | CMD_ASYNC_ENABLE));
        if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED != 0) {
            return Err(Error::InitFailed);
        }
    }
    crate::debug::crumb(0x1004);
    write32(operational + OP_USBCMD, CMD_RESET);
    if !wait_until(100, || read32(operational + OP_USBCMD) & CMD_RESET == 0) {
        return Err(Error::InitFailed);
    }
    crate::debug::crumb(0x1005);
    write32(operational + OP_USBINTR, 0); // polling only

    let frame = pmem::allocate().ok_or(Error::InitFailed)?;
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, pmem::FRAME_SIZE as usize) };
    if hccparams & 1 != 0 {
        write32(operational + OP_CTRLDSSEGMENT, 0); // all our memory is below 4 GiB
    }
    // The asynchronous list: one QH that points at itself and is the head of the
    // reclamation list (H bit), which is what a one-entry list must be.
    let qh = frame + OFFSET_QH;
    unsafe {
        let words = qh as *mut u32;
        core::ptr::write_volatile(words, qh | (1 << 1)); // horizontal link: this QH
        core::ptr::write_volatile(words.add(1), 1 << 15); // H
        core::ptr::write_volatile(words.add(2), 1 << 30); // Mult = 1
        core::ptr::write_volatile(words.add(4), LIST_TERMINATE); // next qTD: none
        core::ptr::write_volatile(words.add(5), LIST_TERMINATE);
    }
    write32(operational + OP_ASYNCLISTADDR, qh);

    let controller = Ehci {
        instance,
        mmio,
        operational,
        ports,
        frame,
        state: UnsafeCell::new(State { addresses: 0, port_addresses: [[0; 8]; 16] }),
    };
    crate::debug::crumb(0x1006);
    write32(operational + OP_USBCMD, CMD_RUN | (8 << 16)); // interrupt threshold: 8 microframes
    if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED == 0) {
        return Err(Error::InitFailed);
    }
    // Route every port to this controller rather than its companions, and power
    // them. Devices need 100 ms to be seen reliably after that.
    crate::debug::crumb(0x1007);
    write32(operational + OP_CONFIGFLAG, 1);
    if hcsparams & (1 << 4) != 0 {
        for port in 1..=ports {
            let register = operational + OP_PORTSC + 4 * (u32::from(port) - 1);
            write32(register, (read32(register) & PORT_KEEP) | PORT_POWER);
        }
    }
    wait_ticks(15);
    crate::debug::crumb(0x1008);
    unsafe {
        CONTROLLERS[instance] = Some(controller);
        CONTROLLER_COUNT += 1;
    }
    Ok(())
}

impl Ehci {
    #[allow(clippy::mut_from_ref)]
    fn state(&self) -> &mut State {
        unsafe { &mut *self.state.get() }
    }

    pub fn instance(&self) -> usize {
        self.instance
    }

    pub fn bar(&self) -> u32 {
        self.mmio
    }

    pub fn port_count(&self) -> u8 {
        self.ports
    }

    fn port_register(&self, port: u8) -> u32 {
        self.operational + OP_PORTSC + 4 * (u32::from(port) - 1)
    }

    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    /// Whether this driver has addressed something on `port` and not let go.
    pub fn port_in_use(&self, port: u8) -> bool {
        self.state().port_addresses.get(usize::from(port)).is_some_and(|slot| slot[0] != 0)
    }

    fn allocate_address(&self) -> Option<u8> {
        let state = self.state();
        (1..=127u8).find(|&address| state.addresses & (1u128 << address) == 0).inspect(|&address| {
            state.addresses |= 1u128 << address;
        })
    }

    /// Frees the addresses given out on a root port (a fresh enumeration is
    /// about to hand out new ones; the old device handles go stale, exactly as
    /// they do for `uhci.rs`).
    pub fn release_port(&self, port: u8) {
        let state = self.state();
        if let Some(slot) = state.port_addresses.get_mut(usize::from(port)) {
            for &address in slot.iter().filter(|&&address| address != 0) {
                state.addresses &= !(1u128 << address);
            }
            *slot = [0; 8];
        }
    }

    /// A fresh device address, remembered as belonging to root port `port`.
    fn new_address(&self, port: u8) -> Result<u8, Error> {
        let address = self.allocate_address().ok_or(Error::NoAddress)?;
        match self.state().port_addresses.get_mut(usize::from(port)).and_then(|slot| slot.iter_mut().find(|entry| **entry == 0)) {
            Some(entry) => {
                *entry = address;
                Ok(address)
            }
            None => {
                self.state().addresses &= !(1u128 << address);
                Err(Error::NoAddress)
            }
        }
    }

    /// The device sitting at the default address 0 (just reset): learn its
    /// default-pipe packet size, give it an address, read its device descriptor.
    fn address_default_device(&self, root_port: u8) -> Result<EnumeratedDevice, Error> {
        let mut first8 = [0u8; 8];
        self.get_descriptor(0, 64, uhci::USB_DESCRIPTOR_DEVICE, 0, &mut first8)?;
        let max_packet0 = u16::from(first8[7]).max(8);
        let address = self.new_address(root_port)?;
        self.set_address(address)?;
        let mut descriptor = [0u8; 18];
        self.get_descriptor(address, max_packet0, uhci::USB_DESCRIPTOR_DEVICE, 0, &mut descriptor)?;
        Ok(EnumeratedDevice { root_port, hub_port: 0, address, max_packet0, descriptor })
    }

    /// Resets a root port and addresses whatever answers, looking one level
    /// through a hub: `visit` gets the device itself, or each device behind a hub
    /// on it (never the hub). Fresh addresses every call; earlier handles to
    /// devices on this port go stale. Full- and low-speed devices are skipped.
    pub fn enumerate_port(&self, root_port: u8, visit: &mut dyn FnMut(&EnumeratedDevice)) -> Result<(), Error> {
        crate::debug::crumb(0x2000 + u32::from(root_port));
        self.release_port(root_port);
        self.reset_port(root_port)?;
        crate::debug::crumb(0x2100 + u32::from(root_port));
        let device = self.address_default_device(root_port)?;
        crate::debug::crumb(0x2200 + u32::from(root_port));
        if device.descriptor[4] != 9 {
            visit(&device);
            return Ok(());
        }
        self.set_configuration(device.address, device.max_packet0, 1)?;
        let hub_ports = self.hub_port_count(device.address)?;
        for hub_port in 1..=hub_ports.min(15) {
            if self.hub_reset_port(device.address, hub_port).is_err() {
                continue;
            }
            if let Ok(mut child) = self.address_default_device(root_port) {
                child.hub_port = hub_port;
                visit(&child);
            }
        }
        Ok(())
    }

    /// Resets a root port with a device on it and leaves a high-speed device
    /// enabled at address 0. A full- or low-speed device is handed to the companion
    /// controller instead (`Error::NotHighSpeed`).
    pub fn reset_port(&self, port: u8) -> Result<(), Error> {
        if port == 0 || port > self.ports {
            return Err(Error::NoDevice);
        }
        let register = self.port_register(port);
        let status = read32(register);
        if status & PORT_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        // Line state K (01) means a low-speed device: not ours.
        if (status >> PORT_LINE_STATUS_SHIFT) & 3 == 1 {
            write32(register, (status & PORT_KEEP) | PORT_OWNER);
            return Err(Error::NotHighSpeed);
        }
        write32(register, (status & PORT_KEEP & !PORT_ENABLED) | PORT_RESET | PORT_CHANGE_BITS);
        wait_ticks(6); // reset must be held at least 50 ms
        write32(register, read32(register) & PORT_KEEP & !PORT_RESET);
        if !wait_until(5, || read32(register) & PORT_RESET == 0) {
            return Err(Error::NoDevice);
        }
        wait_ticks(3); // reset recovery
        let after = read32(register);
        write32(register, (after & PORT_KEEP) | PORT_CHANGE_BITS);
        if after & PORT_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        if after & PORT_ENABLED == 0 {
            // It did not train as high speed: full speed, on the companion's side.
            write32(register, (after & PORT_KEEP) | PORT_OWNER);
            return Err(Error::NotHighSpeed);
        }
        Ok(())
    }

    fn command(&self) -> u32 {
        read32(self.operational + OP_USBCMD)
    }

    fn set_async(&self, on: bool) -> bool {
        let command = self.command();
        write32(self.operational + OP_USBCMD, if on { command | CMD_ASYNC_ENABLE } else { command & !CMD_ASYNC_ENABLE });
        wait_until(5, || (read32(self.operational + OP_USBSTS) & STS_ASYNC_RUNNING != 0) == on)
    }

    /// Runs `qtds` as one queue transfer to `address`/`endpoint`. Returns, for
    /// each qTD, the bytes left untransferred; `Err` if it halted or timed out.
    /// The data toggle after the chain is `toggle_out` (read from the last qTD
    /// the controller completed, which it rewrites).
    fn run(
        &self,
        address: u8,
        endpoint: u8,
        max_packet: u16,
        qtds: &[Qtd],
        ticks: u64,
        toggle_out: &mut bool,
    ) -> Result<[u32; MAX_QTDS], Error> {
        debug_assert!(qtds.len() <= MAX_QTDS && !qtds.is_empty());
        if !self.set_async(false) {
            return Err(Error::Timeout);
        }
        let qtd_address = |index: usize| self.frame + OFFSET_QTDS + index as u32 * QTD_STRIDE;
        for (index, qtd) in qtds.iter().enumerate() {
            let words = qtd_address(index) as *mut u32;
            let next = if index + 1 < qtds.len() { qtd_address(index + 1) } else { LIST_TERMINATE };
            let alternate = qtd.alternate.map_or(LIST_TERMINATE, qtd_address);
            let last = index + 1 == qtds.len();
            let token = (u32::from(qtd.toggle) << 31)
                | (qtd.bytes << 16)
                | (u32::from(last) << 15) // interrupt on complete (polled, but harmless)
                | (3 << 10) // error counter
                | (qtd.pid << 8)
                | TOKEN_ACTIVE;
            unsafe {
                core::ptr::write_volatile(words, next);
                core::ptr::write_volatile(words.add(1), alternate);
                core::ptr::write_volatile(words.add(2), token);
                for page in 0..5u32 {
                    // Buffer pointer 0 carries the offset within its page; the others
                    // are page-aligned continuations.
                    let pointer = if page == 0 { qtd.buffer } else { (qtd.buffer & !0xfff) + page * 0x1000 };
                    core::ptr::write_volatile(words.add(3 + page as usize), pointer);
                }
            }
        }
        // Program the QH for this endpoint and hand it the chain.
        unsafe {
            let words = (self.frame + OFFSET_QH) as *mut u32;
            core::ptr::write_volatile(
                words.add(1),
                u32::from(address)
                    | (u32::from(endpoint & 0xf) << 8)
                    | (2 << 12) // high speed
                    | (1 << 14) // data toggle comes from the qTDs
                    | (1 << 15) // head of reclamation list
                    | (u32::from(max_packet) << 16),
            );
            core::ptr::write_volatile(words.add(2), 1 << 30);
            core::ptr::write_volatile(words.add(3), 0);
            core::ptr::write_volatile(words.add(4), qtd_address(0)); // overlay: next qTD
            core::ptr::write_volatile(words.add(5), LIST_TERMINATE);
            core::ptr::write_volatile(words.add(6), 0); // overlay token: idle
            for buffer in 7..12 {
                core::ptr::write_volatile(words.add(buffer), 0);
            }
        }
        write32(self.operational + OP_ASYNCLISTADDR, self.frame + OFFSET_QH);
        // Clear stale status bits (write 1 to clear), then let the schedule run.
        write32(self.operational + OP_USBSTS, 0x3f);
        let started = self.set_async(true);
        let outcome = if started { self.wait_chain(qtds, ticks, toggle_out) } else { Err(Error::Timeout) };
        self.set_async(false);
        outcome
    }

    /// Polls the chain until every qTD is done, one halted, or a short packet
    /// ended it; returns each qTD's leftover byte count.
    fn wait_chain(&self, qtds: &[Qtd], ticks: u64, toggle_out: &mut bool) -> Result<[u32; MAX_QTDS], Error> {
        let token_of = |index: usize| read32(self.frame + OFFSET_QTDS + index as u32 * QTD_STRIDE + 8);
        let start = interrupts::ticks();
        loop {
            crate::debug::watchdog_pet();
            let mut remaining = [0u32; MAX_QTDS];
            let mut finished = true;
            for index in 0..qtds.len() {
                let token = token_of(index);
                remaining[index] = (token >> 16) & 0x7fff;
                if token & TOKEN_HALTED != 0 {
                    return Err(Error::Transfer((token & 0xff) as u8));
                }
                if token & TOKEN_ACTIVE != 0 {
                    finished = false;
                    break;
                }
                *toggle_out = token >> 31 != 0;
                if remaining[index] != 0 && qtds[index].alternate.is_none() {
                    // A short packet ended this qTD and the queue with it; the
                    // rest of the chain will not run.
                    for later in index + 1..qtds.len() {
                        remaining[later] = qtds[later].bytes;
                    }
                    return Ok(remaining);
                }
            }
            if finished {
                return Ok(remaining);
            }
            if interrupts::ticks().wrapping_sub(start) >= ticks {
                return Err(Error::Timeout);
            }
        }
    }

    /// One control transfer to `address`'s default pipe: SETUP, an optional DATA
    /// stage (bounced through the controller's frame), STATUS. Returns the data
    /// bytes moved.
    pub fn control_transfer(
        &self,
        address: u8,
        max_packet0: u16,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> Result<usize, Error> {
        if data.len() > MAX_CONTROL_BYTES {
            return Err(Error::Unsupported);
        }
        let data_in = request_type & 0x80 != 0;
        let setup = self.frame + OFFSET_SETUP;
        let bounce = self.frame + OFFSET_BOUNCE;
        unsafe {
            let packet = setup as *mut u8;
            let fields: [u8; 8] = [
                request_type,
                request,
                value as u8,
                (value >> 8) as u8,
                index as u8,
                (index >> 8) as u8,
                data.len() as u8,
                (data.len() >> 8) as u8,
            ];
            core::ptr::copy_nonoverlapping(fields.as_ptr(), packet, 8);
            if !data_in && !data.is_empty() {
                core::ptr::copy_nonoverlapping(data.as_ptr(), bounce as *mut u8, data.len());
            }
        }
        let mut qtds = [Qtd { pid: 0, toggle: false, bytes: 0, buffer: 0, alternate: None }; 3];
        let mut count = 0;
        qtds[count] = Qtd { pid: PID_SETUP, toggle: false, bytes: 8, buffer: setup, alternate: None };
        count += 1;
        let data_stage = !data.is_empty();
        if data_stage {
            // A short IN data stage must still run the status stage: continue there.
            qtds[count] = Qtd {
                pid: if data_in { PID_IN } else { PID_OUT },
                toggle: true,
                bytes: data.len() as u32,
                buffer: bounce,
                alternate: Some(count + 1),
            };
            count += 1;
        }
        qtds[count] = Qtd {
            pid: if data_stage && data_in { PID_OUT } else { PID_IN },
            toggle: true,
            bytes: 0,
            buffer: bounce,
            alternate: None,
        };
        count += 1;
        let mut toggle = false;
        let outcome = self.run(address, 0, max_packet0, &qtds[..count], TIMEOUT_TICKS, &mut toggle);
        let remaining = outcome?;
        let moved = if data_stage { data.len() - remaining[1] as usize } else { 0 };
        if data_in && moved != 0 {
            unsafe { core::ptr::copy_nonoverlapping(bounce as *const u8, data.as_mut_ptr(), moved) };
        }
        Ok(moved)
    }

    /// One bulk transfer (`data_in` for an IN endpoint), chunked into qTDs, with
    /// the caller's data toggle kept up to date. Returns the bytes moved.
    pub fn bulk_transfer(
        &self,
        address: u8,
        endpoint: u8,
        max_packet: u16,
        buffer: &mut [u8],
        data_in: bool,
        toggle: &mut bool,
    ) -> Result<usize, Error> {
        if buffer.is_empty() || buffer.len() > MAX_BULK_BYTES {
            return Err(Error::Unsupported);
        }
        let mut qtds = [Qtd { pid: 0, toggle: false, bytes: 0, buffer: 0, alternate: None }; 4];
        let mut count = 0;
        let mut offset = 0;
        // The toggle each qTD starts with follows from how many packets the ones
        // before it sent; the controller then reports the real end state.
        let mut next_toggle = *toggle;
        while offset < buffer.len() {
            let bytes = (buffer.len() - offset).min(QTD_MAX_BYTES);
            qtds[count] = Qtd {
                pid: if data_in { PID_IN } else { PID_OUT },
                toggle: next_toggle,
                bytes: bytes as u32,
                buffer: buffer.as_ptr() as u32 + offset as u32,
                alternate: None,
            };
            let packets = bytes.div_ceil(usize::from(max_packet.max(1)));
            if packets % 2 == 1 {
                next_toggle = !next_toggle;
            }
            count += 1;
            offset += bytes;
        }
        let mut end_toggle = *toggle;
        let result = self.run(address, endpoint, max_packet, &qtds[..count], BULK_TIMEOUT_TICKS, &mut end_toggle);
        match result {
            Ok(remaining) => {
                *toggle = end_toggle;
                let left: u32 = remaining[..count].iter().sum();
                Ok(buffer.len() - left as usize)
            }
            Err(error) => Err(error),
        }
    }

    // ---- standard requests (the same shape `uhci.rs` offers) ----

    pub fn get_descriptor(&self, address: u8, max_packet0: u16, kind: u8, index: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(address, max_packet0, 0x80, 0x06, (u16::from(kind) << 8) | u16::from(index), 0, buffer)
    }

    pub fn set_address(&self, new_address: u8) -> Result<(), Error> {
        self.control_transfer(0, 64, 0x00, 0x05, u16::from(new_address), 0, &mut []).map(|_| ())?;
        wait_ticks(1); // SET_ADDRESS recovery time
        Ok(())
    }

    pub fn set_configuration(&self, address: u8, max_packet0: u16, value: u8) -> Result<(), Error> {
        self.control_transfer(address, max_packet0, 0x00, 0x09, u16::from(value), 0, &mut []).map(|_| ())
    }

    pub fn bulk_only_mass_storage_reset(&self, address: u8, max_packet0: u16, interface: u8) -> Result<(), Error> {
        self.control_transfer(address, max_packet0, 0x21, 0xff, 0, u16::from(interface), &mut []).map(|_| ())
    }

    pub fn clear_endpoint_halt(&self, address: u8, max_packet0: u16, endpoint: u8) -> Result<(), Error> {
        self.control_transfer(address, max_packet0, 0x02, 0x01, 0, u16::from(endpoint), &mut []).map(|_| ())
    }

    // ---- one level of hub ----

    /// Standard hub class requests (bRequestType 0xA0/0x20/0x23 family) to the
    /// hub at `hub`.
    fn hub_request(&self, hub: u8, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(hub, 64, request_type, request, value, index, data)
    }

    /// The number of downstream ports of the (already configured) hub at `hub`.
    pub fn hub_port_count(&self, hub: u8) -> Result<u8, Error> {
        let mut descriptor = [0u8; 9];
        self.hub_request(hub, 0xa0, 0x06, 0x2900, 0, &mut descriptor)?;
        Ok(descriptor[2])
    }

    /// Powers a hub port and reports its status word (bit 0 connected, bit 1
    /// enabled, bit 9 low speed, bit 10 high speed) and change word.
    pub fn hub_port_status(&self, hub: u8, port: u8) -> Result<(u16, u16), Error> {
        let mut status = [0u8; 4];
        self.hub_request(hub, 0xa3, 0x00, 0, u16::from(port), &mut status)?;
        Ok((u16::from_le_bytes([status[0], status[1]]), u16::from_le_bytes([status[2], status[3]])))
    }

    pub fn hub_set_port_feature(&self, hub: u8, port: u8, feature: u16) -> Result<(), Error> {
        self.hub_request(hub, 0x23, 0x03, feature, u16::from(port), &mut []).map(|_| ())
    }

    pub fn hub_clear_port_feature(&self, hub: u8, port: u8, feature: u16) -> Result<(), Error> {
        self.hub_request(hub, 0x23, 0x01, feature, u16::from(port), &mut []).map(|_| ())
    }

    /// Powers a hub port, waits for a device, resets it and leaves a high-speed
    /// device enabled at address 0, like `reset_port` does for a root port.
    pub fn hub_reset_port(&self, hub: u8, port: u8) -> Result<(), Error> {
        const PORT_POWER_FEATURE: u16 = 8;
        const PORT_RESET_FEATURE: u16 = 4;
        const C_PORT_CONNECTION: u16 = 16;
        const C_PORT_RESET: u16 = 20;
        crate::debug::crumb(0x2300 + u32::from(port));
        self.hub_set_port_feature(hub, port, PORT_POWER_FEATURE)?;
        wait_ticks(10); // power-on to power-good, generously
        let (status, _) = self.hub_port_status(hub, port)?;
        if status & 1 == 0 {
            return Err(Error::NoDevice);
        }
        let _ = self.hub_clear_port_feature(hub, port, C_PORT_CONNECTION);
        self.hub_set_port_feature(hub, port, PORT_RESET_FEATURE)?;
        let mut enabled = false;
        for _ in 0..20 {
            wait_ticks(1);
            let (status, change) = self.hub_port_status(hub, port)?;
            if change & (1 << 4) != 0 && status & (1 << 4) == 0 {
                let _ = self.hub_clear_port_feature(hub, port, C_PORT_RESET);
                // Bit 10: high speed. Full/low-speed devices behind a high-speed
                // hub would need transaction translation, which this driver lacks.
                if status & 2 == 0 || status & (1 << 10) == 0 {
                    return Err(Error::NotHighSpeed);
                }
                enabled = true;
                break;
            }
        }
        if !enabled {
            return Err(Error::NoDevice);
        }
        wait_ticks(3);
        Ok(())
    }
}
