//! A minimal xHCI (USB 3.x) host controller driver: enough to find devices on
//! the root ports, address them, run control transfers on their default
//! pipe, and move bulk data -- which is what the Bulk-Only Mass Storage driver
//! (`usb_storage.rs`) needs to read a USB stick natively.
//!
//! Why it exists: a BIOS that boots fos from a USB stick keeps that stick
//! readable through INT13h by emulating it in firmware (SMM) on top of the
//! USB host controller. The moment fos takes the controller over, the
//! emulation dies, so the only way to keep using the boot disk -- and, under
//! UEFI, to read any USB disk at all -- is a native driver. The Skylake test
//! machine has an xHCI controller and nothing else; the Sandy Bridge machine's
//! USB 3 ports are an add-on NEC xHCI. (`uhci.rs` is the USB 1.1 driver.)
//!
//! xHCI in one paragraph. The host controller reads *rings* of 16-byte
//! Transfer Request Blocks (TRBs) from RAM: one command ring (software ->
//! controller), one transfer ring per endpoint (software -> controller) and one
//! event ring (controller -> software). A TRB carries a *cycle bit*; software
//! flips the meaning of that bit each time it wraps a ring, and a link TRB at
//! the end of a ring points back to its start, so neither side needs a
//! separate "valid" flag. After queueing TRBs software rings a *doorbell*
//! register (one per device slot, plus doorbell 0 for the controller itself).
//! Completions come back as event TRBs on the event ring. Per-device state
//! lives in a *device context* the controller owns and software only reads;
//! software changes it by filling an *input context* and issuing an Address
//! Device / Configure Endpoint command. Device context base addresses are
//! listed in one array (the DCBAA) the controller is pointed at.
//!
//! Scope, matching this project's "reject rather than emulate" convention:
//! root-port devices only (no hubs), polling only (no interrupts, no MSI), one
//! event ring segment, no streams, no isochronous transfers, at most
//! `MAX_SLOTS` devices per controller, 4 KiB host pages. All memory comes from
//! `pmem` (identity mapped, below 4 GiB, so a 64-bit-DMA controller and a
//! 32-bit one behave the same).

use core::cell::UnsafeCell;

use crate::interrupts;
use crate::paging;
use crate::pci::{self, PciAddress};
use crate::pmem;
use crate::uhci;

pub const MAX_INSTANCES: usize = 2;
/// Device slots enabled per controller (`MaxSlotsEn`): the number of USB devices
/// this driver can have addressed at once.
pub const MAX_SLOTS: usize = 8;
/// One 4 KiB frame per ring: 256 TRBs, the last of which is a link TRB.
const RING_TRBS: usize = 256;
const TRB_BYTES: u32 = 16;
const FRAME: u32 = pmem::FRAME_SIZE;
/// Endpoint device-context indices run 1..=31 (DCI 1 is the default control pipe).
const DCI_COUNT: usize = 32;
/// Default timeout for a command or control transfer: 1 s at 100 Hz.
const TIMEOUT_TICKS: u64 = 100;
/// A bulk transfer (up to a few sectors) gets longer, as a slow stick may.
const BULK_TIMEOUT_TICKS: u64 = 500;
/// A single TRB's data may neither exceed 64 KiB nor cross a 64 KiB boundary.
const TRB_BOUNDARY: u32 = 64 * 1024;
/// Largest bulk transfer this driver issues in one call.
pub const MAX_BULK_BYTES: usize = 64 * 1024;

// ---- capability registers (offsets from the BAR) ----
const CAP_LENGTH: u32 = 0x00;
const CAP_HCSPARAMS1: u32 = 0x04;
const CAP_HCSPARAMS2: u32 = 0x08;
const CAP_HCCPARAMS1: u32 = 0x10;
const CAP_DBOFF: u32 = 0x14;
const CAP_RTSOFF: u32 = 0x18;

// ---- operational registers (offsets from BAR + CAPLENGTH) ----
const OP_USBCMD: u32 = 0x00;
const OP_USBSTS: u32 = 0x04;
const OP_PAGESIZE: u32 = 0x08;
const OP_CRCR: u32 = 0x18;
const OP_DCBAAP: u32 = 0x30;
const OP_CONFIG: u32 = 0x38;
const OP_PORTS: u32 = 0x400;
const PORT_REGISTER_BYTES: u32 = 0x10;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const STS_HALTED: u32 = 1 << 0;
const STS_NOT_READY: u32 = 1 << 11;

// ---- port status/control bits ----
const PORTSC_CONNECTED: u32 = 1 << 0;
const PORTSC_ENABLED: u32 = 1 << 1;
const PORTSC_RESET: u32 = 1 << 4;
const PORTSC_LINK_STATE_SHIFT: u32 = 5;
const PORTSC_POWER: u32 = 1 << 9;
const PORTSC_SPEED_SHIFT: u32 = 10;
/// The write-1-to-clear change bits (CSC, PEC, WRC, OCC, PRC, PLC, CEC).
const PORTSC_CHANGE_BITS: u32 = 0x7f << 17;
const PORTSC_PRC: u32 = 1 << 21;
/// Bits a read-modify-write of PORTSC must preserve: the read-only status, and
/// the read/write control fields. Writing a 1 to PED would *disable* the port
/// and a 1 to a change bit clears it, so everything else is written as zero.
const PORTSC_PRESERVE: u32 = (1 << 0) | (1 << 3) | (0xf << 10) | (1 << 30) | (0xf << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);

// ---- TRB types ----
const TRB_NORMAL: u32 = 1;
const TRB_SETUP: u32 = 2;
const TRB_DATA: u32 = 3;
const TRB_STATUS: u32 = 4;
const TRB_LINK: u32 = 6;
const TRB_ENABLE_SLOT: u32 = 9;
const TRB_DISABLE_SLOT: u32 = 10;
const TRB_ADDRESS_DEVICE: u32 = 11;
const TRB_CONFIGURE_ENDPOINT: u32 = 12;
const TRB_EVALUATE_CONTEXT: u32 = 13;
const TRB_RESET_ENDPOINT: u32 = 14;
const TRB_SET_TR_DEQUEUE: u32 = 16;
const TRB_NOOP_COMMAND: u32 = 23;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;

const TRB_CYCLE: u32 = 1 << 0;
const TRB_TOGGLE_CYCLE: u32 = 1 << 1;
const TRB_CHAIN: u32 = 1 << 4;
const TRB_IOC: u32 = 1 << 5;
const TRB_IMMEDIATE_DATA: u32 = 1 << 6;
const TRB_ISP: u32 = 1 << 2;
const TRB_DIR_IN: u32 = 1 << 16;

// ---- completion codes ----
const CC_SUCCESS: u8 = 1;
const CC_SHORT_PACKET: u8 = 13;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    NotPresent,
    InitFailed,
    /// No completion event arrived in time.
    Timeout,
    /// The port has no device (or it never came out of reset).
    NoDevice,
    /// A command completed with this completion code (not "success").
    Command(u8),
    /// A transfer completed with this completion code (6 = STALL, 4 = USB
    /// transaction error, ...).
    Transfer(u8),
    /// All of this controller's device slots are in use.
    NoSlot,
    Unsupported,
}

impl Error {
    /// The error in `usb_storage`'s existing vocabulary (it was written for UHCI):
    /// a timeout stays a timeout; anything else carries its xHCI completion code
    /// in the "failed TD control word" field, tagged with bit 24 so it cannot be
    /// mistaken for a UHCI control word.
    pub fn as_uhci(self) -> uhci::Error {
        match self {
            Error::Timeout => uhci::Error::Timeout,
            Error::Command(code) | Error::Transfer(code) => uhci::Error::TransferFailed(0x0100_0000 | u32::from(code)),
            _ => uhci::Error::TransferFailed(0x0100_0000),
        }
    }
}

/// One Transfer Request Block as the hardware sees it.
#[derive(Clone, Copy)]
struct Trb {
    parameter: u64,
    status: u32,
    control: u32,
}

impl Trb {
    fn trb_type(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }

    fn completion_code(&self) -> u8 {
        (self.status >> 24) as u8
    }

    fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }
}

/// A producer ring (command ring or one endpoint's transfer ring): a frame of
/// TRBs whose last entry is a link back to the start.
#[derive(Clone, Copy)]
struct Ring {
    base: u32,
    enqueue: u16,
    cycle: bool,
}

const NO_RING: Ring = Ring { base: 0, enqueue: 0, cycle: true };

impl Ring {
    /// A zeroed ring in a fresh frame, or `None` if no frame is free.
    fn allocate() -> Option<Ring> {
        let base = allocate_zeroed_frame()?;
        let ring = Ring { base, enqueue: 0, cycle: true };
        // The link TRB's own cycle bit is set whenever the ring wraps (see
        // `push`); its toggle-cycle flag tells the controller to flip its
        // expectation as it follows the link.
        write_trb(base + (RING_TRBS as u32 - 1) * TRB_BYTES, &Trb { parameter: u64::from(base), status: 0, control: TRB_LINK << 10 | TRB_TOGGLE_CYCLE });
        Some(ring)
    }

    fn is_allocated(&self) -> bool {
        self.base != 0
    }

    /// Where the next TRB will go, and the cycle bit it will carry.
    fn enqueue_address(&self) -> u32 {
        self.base + u32::from(self.enqueue) * TRB_BYTES
    }

    /// Queues one TRB (`control` without its cycle bit) and returns its
    /// physical address, which is how its completion event names it. When the
    /// write fills the last usable slot, the link TRB after it is handed to the
    /// controller too and the ring's cycle state flips.
    fn push(&mut self, parameter: u64, status: u32, control: u32) -> u32 {
        let at = self.enqueue_address();
        write_trb(at, &Trb { parameter, status, control: control | u32::from(self.cycle) });
        self.enqueue += 1;
        if self.enqueue as usize == RING_TRBS - 1 {
            let link = self.base + (RING_TRBS as u32 - 1) * TRB_BYTES;
            write_trb(link, &Trb { parameter: u64::from(self.base), status: 0, control: TRB_LINK << 10 | TRB_TOGGLE_CYCLE | u32::from(self.cycle) });
            self.cycle = !self.cycle;
            self.enqueue = 0;
        }
        at
    }
}

fn allocate_zeroed_frame() -> Option<u32> {
    let frame = pmem::allocate()?;
    unsafe { core::ptr::write_bytes(frame as *mut u8, 0, FRAME as usize) };
    Some(frame)
}

fn write_trb(address: u32, trb: &Trb) {
    unsafe {
        let pointer = address as *mut u32;
        // The cycle bit lives in the last dword, which the controller watches:
        // write it last so a half-written TRB is never seen as valid.
        core::ptr::write_volatile(pointer, trb.parameter as u32);
        core::ptr::write_volatile(pointer.add(1), (trb.parameter >> 32) as u32);
        core::ptr::write_volatile(pointer.add(2), trb.status);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        core::ptr::write_volatile(pointer.add(3), trb.control);
    }
}

fn read_trb(address: u32) -> Trb {
    unsafe {
        let pointer = address as *const u32;
        let control = core::ptr::read_volatile(pointer.add(3));
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Trb {
            parameter: u64::from(core::ptr::read_volatile(pointer)) | u64::from(core::ptr::read_volatile(pointer.add(1))) << 32,
            status: core::ptr::read_volatile(pointer.add(2)),
            control,
        }
    }
}

fn read32(address: u32) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

fn write32(address: u32, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) }
}

/// 64-bit registers are written as two 32-bit accesses, low half first: not
/// every controller accepts a 64-bit MMIO write.
fn write64(address: u32, value: u64) {
    write32(address, value as u32);
    write32(address + 4, (value >> 32) as u32);
}

fn wait_ticks(count: u64) {
    let start = interrupts::ticks();
    while interrupts::ticks().wrapping_sub(start) < count {}
}

/// Polls `condition` until it is true or `ticks` timer ticks (10 ms each) pass.
fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = interrupts::ticks();
    loop {
        if condition() {
            return true;
        }
        if interrupts::ticks().wrapping_sub(start) >= ticks {
            return condition();
        }
    }
}

/// One addressed device's driver-side state.
#[derive(Clone, Copy)]
struct SlotState {
    used: bool,
    port: u8,
    speed: u8,
    /// Output device context (controller-owned) and input context (ours).
    output: u32,
    input: u32,
    /// Transfer ring per device-context index (index 0 unused).
    rings: [Ring; DCI_COUNT],
}

const EMPTY_SLOT: SlotState = SlotState { used: false, port: 0, speed: 0, output: 0, input: 0, rings: [NO_RING; DCI_COUNT] };

struct State {
    command_ring: Ring,
    event_index: u16,
    event_cycle: bool,
    slots: [SlotState; MAX_SLOTS + 1],
    /// Port number (1-based) -> USB major revision from the supported-protocol
    /// extended capability (2 or 3), 0 when unknown.
    port_major: [u8; 32],
}

/// A discovered, running controller. All mutable state is behind an
/// `UnsafeCell`: the kernel is single-threaded and no interrupt handler touches
/// a controller, so there is never concurrent access.
pub struct Xhci {
    instance: usize,
    /// BAR base (virtual = physical) and the three register blocks.
    mmio: u32,
    operational: u32,
    runtime: u32,
    doorbells: u32,
    ports: u8,
    /// Context records are 64 bytes instead of 32 when HCCPARAMS1.CSZ is set.
    context_bytes: u32,
    event_ring: u32,
    dcbaa: u32,
    state: UnsafeCell<State>,
}

/// A device that has been given a slot and an address.
#[derive(Clone, Copy)]
pub struct Device {
    pub slot: u8,
    pub port: u8,
    /// PORTSC speed ID: 1 full, 2 low, 3 high, 4 SuperSpeed, 5 SuperSpeed+.
    pub speed: u8,
    /// Default-pipe max packet size currently programmed.
    pub control_max_packet: u16,
}

static mut CONTROLLERS: [Option<Xhci>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CONTROLLER_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

/// Maps a PORTSC speed ID to a printable name.
pub fn speed_name(speed: u8) -> &'static str {
    match speed {
        1 => "full-speed",
        2 => "low-speed",
        3 => "high-speed",
        4 => "SuperSpeed",
        5 => "SuperSpeed+",
        _ => "unknown speed",
    }
}

/// Starts every xHCI controller (up to `MAX_INSTANCES`): idempotent and lazily
/// triggered, like `uhci::initialize_all`. **Taking a controller over ends the
/// BIOS's INT13h emulation of any USB disk behind it**, so nothing calls this
/// at boot; the `usb` commands do.
pub fn initialize_all() -> Result<usize, Error> {
    unsafe {
        if INITIALIZED {
            return if CONTROLLER_COUNT == 0 { Err(Error::NotPresent) } else { Ok(CONTROLLER_COUNT) };
        }
        INITIALIZED = true;
    }
    let mut matches: [Option<PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_xhci_controllers(&mut matches);
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

pub fn get(index: usize) -> Option<&'static Xhci> {
    initialize_all().ok()?;
    unsafe {
        if index >= CONTROLLER_COUNT {
            return None;
        }
        (*core::ptr::addr_of!(CONTROLLERS))[index].as_ref()
    }
}

/// Takes ownership from the BIOS if the controller has the "USB Legacy
/// Support" extended capability: set the OS-owned semaphore, wait for the
/// BIOS-owned one to clear, then switch off the firmware's SMI sources so it
/// cannot grab the controller back. Skipping this on real hardware leaves the
/// firmware fighting us for the controller.
fn take_ownership_from_bios(mmio: u32, hccparams1: u32) {
    let mut offset = (hccparams1 >> 16) * 4;
    for _ in 0..48 {
        if offset == 0 {
            return;
        }
        let header = read32(mmio + offset);
        if header & 0xff == 1 {
            const BIOS_OWNED: u32 = 1 << 16;
            const OS_OWNED: u32 = 1 << 24;
            if header & BIOS_OWNED != 0 {
                write32(mmio + offset, header | OS_OWNED);
                wait_until(100, || read32(mmio + offset) & BIOS_OWNED == 0);
            }
            // USBLEGCTLSTS: clear the SMI enables (bits 0, 4, 13..=15, 16..=20
            // in the layout xHCI shares with EHCI) and acknowledge the
            // write-1-to-clear status bits 29..=31.
            let control = read32(mmio + offset + 4);
            const SMI_ENABLES: u32 = (0x3 << 1) | (0xff << 5) | (0x7 << 17);
            write32(mmio + offset + 4, (control & !SMI_ENABLES) | (0x7 << 29));
            return;
        }
        let next = (header >> 8) & 0xff;
        if next == 0 {
            return;
        }
        offset += next * 4;
    }
}

/// Records, for every root port, whether it is a USB 2 or USB 3 port (the
/// supported-protocol extended capability, ID 2, lists port ranges).
fn read_port_protocols(mmio: u32, hccparams1: u32, ports: u8, major_of: &mut [u8; 32]) {
    let mut offset = (hccparams1 >> 16) * 4;
    for _ in 0..48 {
        if offset == 0 {
            return;
        }
        let header = read32(mmio + offset);
        if header & 0xff == 2 {
            let major = (header >> 24) as u8;
            let ports_dword = read32(mmio + offset + 8);
            let first = (ports_dword & 0xff) as usize;
            let count = ((ports_dword >> 8) & 0xff) as usize;
            for port in first..first + count {
                if port >= 1 && port <= usize::from(ports) && port < major_of.len() {
                    major_of[port] = major;
                }
            }
        }
        let next = (header >> 8) & 0xff;
        if next == 0 {
            return;
        }
        offset += next * 4;
    }
}

fn init_controller(address: PciAddress) -> Result<(), Error> {
    let instance = unsafe { CONTROLLER_COUNT };
    if instance >= MAX_INSTANCES {
        return Err(Error::InitFailed);
    }
    // A function a BIOS left in D3 would not answer MMIO at all.
    address.force_power_state_d0();
    address.enable_memory_and_bus_master();
    let (base, size) = address.memory_bar_region(0).ok_or(Error::InitFailed)?;
    paging::map_mmio(base, size).map_err(|_| Error::InitFailed)?;

    let mmio = base;
    let cap_length = u32::from(unsafe { core::ptr::read_volatile(mmio as *const u8) });
    let hcsparams1 = read32(mmio + CAP_HCSPARAMS1);
    let hcsparams2 = read32(mmio + CAP_HCSPARAMS2);
    let hccparams1 = read32(mmio + CAP_HCCPARAMS1);
    let operational = mmio + cap_length;
    let doorbells = mmio + (read32(mmio + CAP_DBOFF) & !3);
    let runtime = mmio + (read32(mmio + CAP_RTSOFF) & !0x1f);
    let max_slots = hcsparams1 & 0xff;
    let ports = ((hcsparams1 >> 24) & 0xff) as u8;
    if max_slots == 0 || ports == 0 || ports > 31 {
        return Err(Error::InitFailed);
    }
    let _ = CAP_LENGTH;

    take_ownership_from_bios(mmio, hccparams1);

    // Halt, then reset: HCRST returns every register to its power-on value and
    // the controller reports "not ready" until it is done.
    if !wait_until(100, || read32(operational + OP_USBSTS) & STS_NOT_READY == 0) {
        return Err(Error::InitFailed);
    }
    let command = read32(operational + OP_USBCMD);
    if command & CMD_RUN != 0 {
        write32(operational + OP_USBCMD, command & !CMD_RUN);
        if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED != 0) {
            return Err(Error::InitFailed);
        }
    }
    write32(operational + OP_USBCMD, CMD_RESET);
    if !wait_until(100, || read32(operational + OP_USBCMD) & CMD_RESET == 0 && read32(operational + OP_USBSTS) & STS_NOT_READY == 0) {
        return Err(Error::InitFailed);
    }
    // This driver only speaks 4 KiB host pages (PAGESIZE bit 0).
    if read32(operational + OP_PAGESIZE) & 1 == 0 {
        return Err(Error::InitFailed);
    }

    let slots_enabled = max_slots.min(MAX_SLOTS as u32);
    write32(operational + OP_CONFIG, slots_enabled);

    // The Device Context Base Address Array: entry 0 points at the scratchpad
    // buffer array (pages the controller wants for its own use), entries
    // 1..=slots at each device's output context.
    let dcbaa = allocate_zeroed_frame().ok_or(Error::InitFailed)?;
    let scratchpads = (((hcsparams2 >> 21) & 0x1f) << 5) | ((hcsparams2 >> 27) & 0x1f);
    if scratchpads != 0 {
        if scratchpads > 512 {
            return Err(Error::InitFailed);
        }
        let array = allocate_zeroed_frame().ok_or(Error::InitFailed)?;
        for index in 0..scratchpads {
            let page = allocate_zeroed_frame().ok_or(Error::InitFailed)?;
            unsafe { core::ptr::write_volatile((array as *mut u64).add(index as usize), u64::from(page)) };
        }
        unsafe { core::ptr::write_volatile(dcbaa as *mut u64, u64::from(array)) };
    }
    write64(operational + OP_DCBAAP, u64::from(dcbaa));

    // Command ring: CRCR holds its start and the ring cycle state (bit 0).
    let command_ring = Ring::allocate().ok_or(Error::InitFailed)?;
    write64(operational + OP_CRCR, u64::from(command_ring.base) | 1);

    // Event ring (interrupter 0): a one-entry Event Ring Segment Table points
    // at one frame of event TRBs. ERSTSZ and ERDP must be written before ERSTBA,
    // whose write is what arms the ring.
    let event_ring = allocate_zeroed_frame().ok_or(Error::InitFailed)?;
    let segment_table = allocate_zeroed_frame().ok_or(Error::InitFailed)?;
    unsafe {
        let entry = segment_table as *mut u32;
        core::ptr::write_volatile(entry, event_ring);
        core::ptr::write_volatile(entry.add(2), RING_TRBS as u32);
    }
    let interrupter = runtime + 0x20;
    write32(interrupter + 0x08, 1);
    write64(interrupter + 0x18, u64::from(event_ring));
    write64(interrupter + 0x10, u64::from(segment_table));

    let mut state = State {
        command_ring,
        event_index: 0,
        event_cycle: true,
        slots: [EMPTY_SLOT; MAX_SLOTS + 1],
        port_major: [0; 32],
    };
    read_port_protocols(mmio, hccparams1, ports, &mut state.port_major);

    let controller = Xhci {
        instance,
        mmio,
        operational,
        runtime,
        doorbells,
        ports,
        context_bytes: if hccparams1 & (1 << 2) != 0 { 64 } else { 32 },
        event_ring,
        dcbaa,
        state: UnsafeCell::new(state),
    };

    // Run (no interrupts: everything here polls the event ring).
    write32(operational + OP_USBCMD, CMD_RUN);
    if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED == 0) {
        return Err(Error::InitFailed);
    }
    // A command-ring smoke test: a No-Op command must complete.
    unsafe {
        CONTROLLERS[instance] = Some(controller);
        CONTROLLER_COUNT += 1;
        let installed = (*core::ptr::addr_of!(CONTROLLERS))[instance].as_ref().ok_or(Error::InitFailed)?;
        if installed.command(0, 0, TRB_NOOP_COMMAND << 10).is_err() {
            CONTROLLERS[instance] = None;
            CONTROLLER_COUNT -= 1;
            return Err(Error::InitFailed);
        }
        // Power every root port that is not powered yet; a device already
        // connected keeps its connection status.
        for port in 1..=ports {
            let status = read32(installed.port_register(port));
            if status & PORTSC_POWER == 0 {
                write32(installed.port_register(port), (status & PORTSC_PRESERVE) | PORTSC_POWER);
            }
        }
    }
    // The controller reset dropped every link: give SuperSpeed devices their
    // link-training time (and freshly powered ports theirs) before anyone looks
    // at port status.
    wait_ticks(15);
    Ok(())
}

impl Xhci {
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

    /// USB major revision (2 or 3) of a root port, 0 if the controller did not say.
    pub fn port_major(&self, port: u8) -> u8 {
        self.state().port_major.get(usize::from(port)).copied().unwrap_or(0)
    }

    /// The slot of a device this driver has already addressed on `port`, if any.
    /// Whoever holds that slot (a `diskN` using the native driver, say) loses it
    /// the moment the port is opened again, so a diagnostic must check first.
    pub fn slot_in_use_for_port(&self, port: u8) -> Option<u8> {
        let state = self.state();
        (1..=MAX_SLOTS as u8).find(|&slot| state.slots[usize::from(slot)].used && state.slots[usize::from(slot)].port == port)
    }

    fn port_register(&self, port: u8) -> u32 {
        self.operational + OP_PORTS + (u32::from(port) - 1) * PORT_REGISTER_BYTES
    }

    /// Raw PORTSC of a root port (1-based).
    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        write32(self.doorbells + u32::from(slot) * 4, target);
    }

    /// Pops the next event, if the controller has written one: its cycle bit
    /// matches our consumer cycle state. Updates the event ring dequeue pointer
    /// (ERDP) and acknowledges the "event handler busy" flag.
    fn next_event(&self) -> Option<Trb> {
        let state = self.state();
        let at = self.event_ring + u32::from(state.event_index) * TRB_BYTES;
        let trb = read_trb(at);
        if (trb.control & TRB_CYCLE != 0) != state.event_cycle {
            return None;
        }
        state.event_index += 1;
        if state.event_index as usize == RING_TRBS {
            state.event_index = 0;
            state.event_cycle = !state.event_cycle;
        }
        let dequeue = self.event_ring + u32::from(state.event_index) * TRB_BYTES;
        // Bit 3 (EHB) is write-1-to-clear.
        write64(self.runtime + 0x20 + 0x18, u64::from(dequeue) | (1 << 3));
        Some(trb)
    }

    /// Waits for an event satisfying `wanted`, discarding the rest (port status
    /// changes and stale completions nobody is waiting for).
    fn wait_event(&self, ticks: u64, mut wanted: impl FnMut(&Trb) -> bool) -> Result<Trb, Error> {
        let start = interrupts::ticks();
        loop {
            while let Some(event) = self.next_event() {
                if wanted(&event) {
                    return Ok(event);
                }
            }
            if interrupts::ticks().wrapping_sub(start) >= ticks {
                return Err(Error::Timeout);
            }
        }
    }

    /// Queues one command TRB, rings doorbell 0 and waits for its completion
    /// event. Returns that event (its completion code is already checked: only
    /// success comes back as `Ok`).
    fn command(&self, parameter: u64, status: u32, control: u32) -> Result<Trb, Error> {
        let at = self.state().command_ring.push(parameter, status, control);
        self.ring_doorbell(0, 0);
        let event = self.wait_event(TIMEOUT_TICKS, |event| {
            event.trb_type() == TRB_COMMAND_COMPLETION && event.parameter == u64::from(at)
        })?;
        match event.completion_code() {
            CC_SUCCESS => Ok(event),
            code => Err(Error::Command(code)),
        }
    }

    fn context_slot(&self, context: u32) -> u32 {
        context + self.context_bytes
    }

    fn context_endpoint(&self, context: u32, dci: u32) -> u32 {
        context + self.context_bytes * (1 + dci)
    }

    /// Resets the port (when a device is attached), gives the device a slot and
    /// an address, and returns a handle. Only root ports; no hubs.
    pub fn open_port(&self, port: u8) -> Result<Device, Error> {
        if port == 0 || port > self.ports {
            return Err(Error::NoDevice);
        }
        let register = self.port_register(port);
        let mut status = read32(register);
        if status & PORTSC_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        // Acknowledge stale change flags, then reset unless a SuperSpeed port has
        // already trained its link (enabled, link state U0).
        write32(register, (status & PORTSC_PRESERVE) | PORTSC_CHANGE_BITS);
        let major = self.port_major(port);
        let trained = status & PORTSC_ENABLED != 0 && (status >> PORTSC_LINK_STATE_SHIFT) & 0xf == 0;
        if !(major == 3 && trained) {
            write32(register, (read32(register) & PORTSC_PRESERVE) | PORTSC_RESET);
            if !wait_until(100, || read32(register) & PORTSC_PRC != 0) {
                return Err(Error::NoDevice);
            }
            write32(register, (read32(register) & PORTSC_PRESERVE) | PORTSC_CHANGE_BITS);
            wait_ticks(3); // the 10 ms reset recovery time
        }
        status = read32(register);
        if status & PORTSC_ENABLED == 0 || status & PORTSC_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        let speed = ((status >> PORTSC_SPEED_SHIFT) & 0xf) as u8;
        let max_packet: u16 = match speed {
            2 => 8,
            1 | 3 => 64,
            _ => 512,
        };

        // Opening a port again (every `usb storage`/`usb native` re-discovers
        // from scratch) replaces its old slot rather than leaking it.
        for old in 1..=MAX_SLOTS as u8 {
            let entry = &self.state().slots[usize::from(old)];
            if entry.used && entry.port == port {
                self.release_slot(old);
            }
        }
        let event = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        let slot = event.slot();
        if slot == 0 || usize::from(slot) > MAX_SLOTS {
            return Err(Error::NoSlot);
        }
        let result = self.address_device(slot, port, speed, max_packet);
        if result.is_err() {
            self.release_slot(slot);
        }
        result
    }

    fn address_device(&self, slot: u8, port: u8, speed: u8, max_packet: u16) -> Result<Device, Error> {
        let input = allocate_zeroed_frame().ok_or(Error::NoSlot)?;
        let output = match allocate_zeroed_frame() {
            Some(frame) => frame,
            None => {
                pmem::free(input);
                return Err(Error::NoSlot);
            }
        };
        let control_ring = match Ring::allocate() {
            Some(ring) => ring,
            None => {
                pmem::free(input);
                pmem::free(output);
                return Err(Error::NoSlot);
            }
        };
        {
            let state = self.state();
            let entry = &mut state.slots[usize::from(slot)];
            *entry = EMPTY_SLOT;
            entry.used = true;
            entry.port = port;
            entry.speed = speed;
            entry.input = input;
            entry.output = output;
            entry.rings[1] = control_ring;
        }
        unsafe { core::ptr::write_volatile((self.dcbaa as *mut u64).add(usize::from(slot)), u64::from(output)) };

        // Input context: add the slot context (A0) and the default pipe (A1).
        unsafe {
            let control = input as *mut u32;
            core::ptr::write_volatile(control.add(1), 0b11);
            let slot_context = self.context_slot(input) as *mut u32;
            // Context entries = 1 (just DCI 1), speed, root hub port number.
            core::ptr::write_volatile(slot_context, (1 << 27) | (u32::from(speed) << 20));
            core::ptr::write_volatile(slot_context.add(1), u32::from(port) << 16);
            let endpoint = self.context_endpoint(input, 1) as *mut u32;
            // CErr = 3, endpoint type 4 (control), max packet size.
            core::ptr::write_volatile(endpoint.add(1), (3 << 1) | (4 << 3) | (u32::from(max_packet) << 16));
            core::ptr::write_volatile(endpoint.add(2), control_ring.base | 1); // dequeue pointer + DCS
            core::ptr::write_volatile(endpoint.add(4), 8); // average TRB length
        }
        self.command(u64::from(input), 0, (TRB_ADDRESS_DEVICE << 10) | (u32::from(slot) << 24))?;
        Ok(Device { slot, port, speed, control_max_packet: max_packet })
    }

    /// Frees a slot's driver-side state (and tells the controller).
    pub fn release_slot(&self, slot: u8) {
        let _ = self.command(0, 0, (TRB_DISABLE_SLOT << 10) | (u32::from(slot) << 24));
        let entry = &mut self.state().slots[usize::from(slot)];
        if entry.used {
            for ring in entry.rings.iter().filter(|ring| ring.is_allocated()) {
                pmem::free(ring.base);
            }
            pmem::free(entry.input);
            pmem::free(entry.output);
            unsafe { core::ptr::write_volatile((self.dcbaa as *mut u64).add(usize::from(slot)), 0) };
        }
        *entry = EMPTY_SLOT;
    }

    /// Tells the controller the default pipe's max packet size is different from
    /// the guess made at address time (the device descriptor's `bMaxPacketSize0`
    /// of a full- or low-speed device).
    pub fn set_control_max_packet(&self, device: &mut Device, max_packet: u16) -> Result<(), Error> {
        if max_packet == device.control_max_packet {
            return Ok(());
        }
        let input = self.state().slots[usize::from(device.slot)].input;
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, FRAME as usize);
            core::ptr::write_volatile((input as *mut u32).add(1), 0b10); // A1: the default pipe
            let endpoint = self.context_endpoint(input, 1) as *mut u32;
            core::ptr::write_volatile(endpoint.add(1), (3 << 1) | (4 << 3) | (u32::from(max_packet) << 16));
        }
        self.command(u64::from(input), 0, (TRB_EVALUATE_CONTEXT << 10) | (u32::from(device.slot) << 24))?;
        device.control_max_packet = max_packet;
        Ok(())
    }

    /// One control transfer on the default pipe: SETUP, an optional DATA stage
    /// through a bounce buffer (so the caller's buffer may sit anywhere), STATUS.
    /// Returns the bytes transferred by the data stage.
    pub fn control_transfer(
        &self,
        device: &Device,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
    ) -> Result<usize, Error> {
        let data_in = request_type & 0x80 != 0;
        if data.len() > 1024 {
            return Err(Error::Unsupported);
        }
        let bounce = allocate_zeroed_frame().ok_or(Error::Unsupported)?;
        let result = self.control_transfer_inner(device, request_type, request, value, index, data, data_in, bounce);
        pmem::free(bounce);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn control_transfer_inner(
        &self,
        device: &Device,
        request_type: u8,
        request: u8,
        value: u16,
        index: u16,
        data: &mut [u8],
        data_in: bool,
        bounce: u32,
    ) -> Result<usize, Error> {
        let length = data.len() as u32;
        if !data_in && length != 0 {
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), bounce as *mut u8, data.len()) };
        }
        let setup = u64::from(request_type)
            | u64::from(request) << 8
            | u64::from(value) << 16
            | u64::from(index) << 32
            | u64::from(length) << 48;
        let transfer_type: u32 = match (length, data_in) {
            (0, _) => 0,
            (_, true) => 3,
            (_, false) => 2,
        };
        let ring = &mut self.state().slots[usize::from(device.slot)].rings[1];
        ring.push(setup, 8, (TRB_SETUP << 10) | TRB_IMMEDIATE_DATA | (transfer_type << 16));
        if length != 0 {
            ring.push(u64::from(bounce), length, (TRB_DATA << 10) | TRB_ISP | if data_in { TRB_DIR_IN } else { 0 });
        }
        // The status stage runs the other way from the data (and "in" when there
        // is no data stage).
        let status_in = !(length != 0 && data_in);
        let last = ring.push(0, 0, (TRB_STATUS << 10) | TRB_IOC | if status_in { TRB_DIR_IN } else { 0 });
        self.ring_doorbell(device.slot, 1);
        let short = self.wait_transfer(device.slot, 1, last, false, TIMEOUT_TICKS)?;
        let transferred = match short {
            Some((_, residual)) => (length as usize).saturating_sub(residual as usize),
            None => length as usize,
        };
        if data_in && length != 0 {
            let copy = transferred.min(data.len());
            unsafe { core::ptr::copy_nonoverlapping(bounce as *const u8, data.as_mut_ptr(), copy) };
        }
        Ok(transferred)
    }

    /// Waits for the TD whose last TRB is at `last` to finish. Returns where a
    /// short packet cut it off, if one did: `(TRB address, bytes left over in
    /// that TRB)`. A short IN data stage is not an error. A stall or transaction
    /// error comes back as `Error::Transfer(code)`; the caller must then reset
    /// the endpoint.
    ///
    /// `short_ends_td`: whether a short packet ends the whole wait. A bulk
    /// transfer is one TD of chained TRBs, and a short packet in any of them
    /// skips the rest of the TD with no further event; a control transfer's
    /// status stage is a TD of its own, so after a short data stage there is
    /// still a final event for `last`.
    fn wait_transfer(&self, slot: u8, dci: u32, last: u32, short_ends_td: bool, ticks: u64) -> Result<Option<(u32, u32)>, Error> {
        let mut short = None;
        loop {
            let event = self.wait_event(ticks, |event| {
                event.trb_type() == TRB_TRANSFER_EVENT
                    && event.slot() == slot
                    && ((event.control >> 16) & 0x1f) == dci
            })?;
            let residual = event.status & 0x00ff_ffff;
            match event.completion_code() {
                CC_SUCCESS if event.parameter == u64::from(last) => return Ok(short),
                CC_SHORT_PACKET => {
                    short = Some((event.parameter as u32, residual));
                    if short_ends_td || event.parameter == u64::from(last) {
                        return Ok(short);
                    }
                }
                CC_SUCCESS => {}
                code => return Err(Error::Transfer(code)),
            }
        }
    }

    /// Adds a device's bulk endpoints to the controller's view of it (the
    /// Configure Endpoint command). `in_endpoint`/`out_endpoint` are endpoint
    /// addresses (bit 7 = IN) with their max packet size and SuperSpeed burst.
    pub fn configure_bulk_endpoints(
        &self,
        device: &Device,
        in_endpoint: u8,
        in_max_packet: u16,
        in_burst: u8,
        out_endpoint: u8,
        out_max_packet: u16,
        out_burst: u8,
    ) -> Result<(), Error> {
        let slot = usize::from(device.slot);
        let in_dci = u32::from(in_endpoint & 0x0f) * 2 + 1;
        let out_dci = u32::from(out_endpoint & 0x0f) * 2;
        if in_dci >= DCI_COUNT as u32 || out_dci == 0 || out_dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let in_ring = Ring::allocate().ok_or(Error::NoSlot)?;
        let out_ring = match Ring::allocate() {
            Some(ring) => ring,
            None => {
                pmem::free(in_ring.base);
                return Err(Error::NoSlot);
            }
        };
        let (input, output) = {
            let entry = &self.state().slots[slot];
            (entry.input, entry.output)
        };
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, FRAME as usize);
            let control = input as *mut u32;
            // Add the slot context and both endpoints.
            core::ptr::write_volatile(control.add(1), 1 | (1 << in_dci) | (1 << out_dci));
            // The slot context is the controller's current one, with "context
            // entries" raised to cover the highest endpoint now in use.
            core::ptr::copy_nonoverlapping(output as *const u8, self.context_slot(input) as *mut u8, 32);
            let slot_context = self.context_slot(input) as *mut u32;
            let entries = in_dci.max(out_dci);
            let word0 = core::ptr::read_volatile(slot_context) & !(0x1f << 27);
            core::ptr::write_volatile(slot_context, word0 | (entries << 27));
            for (dci, ring, endpoint_type, max_packet, burst) in
                [(in_dci, in_ring, 6u32, in_max_packet, in_burst), (out_dci, out_ring, 2u32, out_max_packet, out_burst)]
            {
                let endpoint = self.context_endpoint(input, dci) as *mut u32;
                core::ptr::write_volatile(
                    endpoint.add(1),
                    (3 << 1) | (endpoint_type << 3) | (u32::from(burst) << 8) | (u32::from(max_packet) << 16),
                );
                core::ptr::write_volatile(endpoint.add(2), ring.base | 1);
                core::ptr::write_volatile(endpoint.add(4), 1024);
            }
        }
        {
            let entry = &mut self.state().slots[slot];
            entry.rings[in_dci as usize] = in_ring;
            entry.rings[out_dci as usize] = out_ring;
        }
        self.command(u64::from(input), 0, (TRB_CONFIGURE_ENDPOINT << 10) | (u32::from(device.slot) << 24))?;
        Ok(())
    }

    /// One bulk transfer (IN when bit 7 of `endpoint` is set), possibly several
    /// TRBs because none may cross a 64 KiB boundary. Returns the bytes moved.
    pub fn bulk_transfer(&self, device: &Device, endpoint: u8, buffer: &mut [u8], max_packet: u16) -> Result<usize, Error> {
        let data_in = endpoint & 0x80 != 0;
        let dci = u32::from(endpoint & 0x0f) * 2 + u32::from(data_in);
        if buffer.is_empty() || buffer.len() > MAX_BULK_BYTES || dci == 0 || dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let ring = &mut self.state().slots[usize::from(device.slot)].rings[dci as usize];
        if !ring.is_allocated() {
            return Err(Error::Unsupported);
        }
        let start = buffer.as_ptr() as u32;
        let total = buffer.len() as u32;
        let mut offset = 0u32;
        let mut last = 0;
        // Each TRB's (address, offset into the buffer, length), to turn a short
        // packet's TRB address back into a byte count.
        let mut pieces = [(0u32, 0u32, 0u32); 17];
        let mut piece_count = 0;
        while offset < total {
            let address = start + offset;
            let to_boundary = TRB_BOUNDARY - (address % TRB_BOUNDARY);
            let length = (total - offset).min(to_boundary);
            let remaining_after = total - offset - length;
            let is_last = remaining_after == 0;
            // TD size: packets still to come after this TRB, capped at 31.
            let td_size = (remaining_after.div_ceil(u32::from(max_packet.max(1)))).min(31);
            let mut control = TRB_NORMAL << 10;
            if !is_last {
                control |= TRB_CHAIN;
            } else {
                control |= TRB_IOC;
            }
            if data_in {
                control |= TRB_ISP;
            }
            last = ring.push(u64::from(address), length | (td_size << 17), control);
            pieces[piece_count] = (last, offset, length);
            piece_count += 1;
            offset += length;
        }
        self.ring_doorbell(device.slot, dci);
        let short = self.wait_transfer(device.slot, dci, last, true, BULK_TIMEOUT_TICKS)?;
        Ok(match short {
            Some((trb, residual)) => pieces[..piece_count]
                .iter()
                .find(|piece| piece.0 == trb)
                .map_or(total as usize, |&(_, at, length)| (at + length.saturating_sub(residual)) as usize),
            None => total as usize,
        })
    }

    /// After a stalled or failed transfer: tells the controller to un-halt the
    /// endpoint and skip whatever was left on its ring (the Reset Endpoint and
    /// Set TR Dequeue Pointer commands). Safe on an endpoint that is not halted
    /// (the commands then fail harmlessly).
    pub fn reset_endpoint(&self, device: &Device, endpoint: u8) {
        let data_in = endpoint & 0x80 != 0;
        let dci = u32::from(endpoint & 0x0f) * 2 + u32::from(data_in);
        if dci == 0 || dci >= DCI_COUNT as u32 {
            return;
        }
        let ring = self.state().slots[usize::from(device.slot)].rings[dci as usize];
        if !ring.is_allocated() {
            return;
        }
        let _ = self.command(0, 0, (TRB_RESET_ENDPOINT << 10) | (dci << 16) | (u32::from(device.slot) << 24));
        let dequeue = u64::from(ring.enqueue_address()) | u64::from(ring.cycle);
        let _ = self.command(dequeue, 0, (TRB_SET_TR_DEQUEUE << 10) | (dci << 16) | (u32::from(device.slot) << 24));
    }

    /// `GET_DESCRIPTOR` (standard, device-to-host).
    pub fn get_descriptor(&self, device: &Device, kind: u8, index: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(device, 0x80, 0x06, (u16::from(kind) << 8) | u16::from(index), 0, buffer)
    }

    /// `SET_CONFIGURATION`.
    pub fn set_configuration(&self, device: &Device, value: u8) -> Result<(), Error> {
        self.control_transfer(device, 0x00, 0x09, u16::from(value), 0, &mut []).map(|_| ())
    }

    /// Bulk-Only Mass Storage Reset (class request 0xFF) on `interface`.
    pub fn bulk_only_mass_storage_reset(&self, device: &Device, interface: u8) -> Result<(), Error> {
        self.control_transfer(device, 0x21, 0xff, 0, u16::from(interface), &mut []).map(|_| ())
    }

    /// `CLEAR_FEATURE(ENDPOINT_HALT)` on `endpoint`, plus the controller-side
    /// endpoint reset that must accompany it.
    pub fn clear_endpoint_halt(&self, device: &Device, endpoint: u8) -> Result<(), Error> {
        self.reset_endpoint(device, endpoint);
        self.control_transfer(device, 0x02, 0x01, 0, u16::from(endpoint), &mut []).map(|_| ())
    }
}
