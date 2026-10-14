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
const OP_FRINDEX: usize = 0x0c;
const OP_PERIODICLISTBASE: usize = 0x14;
const OP_ASYNCLISTADDR: usize = 0x18;
const OP_CONFIGFLAG: usize = 0x40;
const OP_PORTSC: usize = 0x44;

const CMD_RUN: u32 = 1 << 0;
const CMD_RESET: u32 = 1 << 1;
const CMD_PERIODIC_ENABLE: u32 = 1 << 4;
const CMD_ASYNC_ENABLE: u32 = 1 << 5;
const CMD_IAAD: u32 = 1 << 6;
const STS_USBINT: u32 = 1 << 0;
const STS_USBERRINT: u32 = 1 << 1;
const STS_HSE: u32 = 1 << 4;
const STS_IAA: u32 = 1 << 5;
const STS_HALTED: u32 = 1 << 12;
const STS_PERIODIC_RUNNING: u32 = 1 << 14;
const STS_ASYNC_RUNNING: u32 = 1 << 15;
const PERIODIC_ENTRIES: usize = 1024;

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
#[allow(dead_code)] // payloads are shown through Debug
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
    #[allow(dead_code)]
    const DEFAULT: Target = Target { address: 0, max_packet0: 64, speed: Speed::High, hub_address: 0, hub_port: 0 };
}

/// QH dword 1: device address, endpoint, endpoint speed, data toggle from the qTD,
/// head-of-list flag, max packet size, and the "control endpoint" flag that a
/// non-high-speed control endpoint must carry.
/// How many iTDs each stream keeps in the periodic list: one per frame, revisited every `ISO_RING` ms (it must divide the
/// 1024 frame-list entries). A refilled iTD plays that long after it was filled, which is the playback latency.
pub const ISO_RING: usize = 16;
/// An iTD is 64 bytes, plus seven extended-buffer-pointer dwords (high address halves) on a controller with 64-bit
/// addressing: 92 bytes, so the stride is 128 (32-byte aligned). With a 64-byte stride the high words of iTD k are the
/// status words of iTD k+1: the controller then DMAs to garbage addresses (only the last iTD of a page works).
pub const ITD_BYTES: usize = 128;

/// Builds an isochronous transfer descriptor. An iTD is one frame of a high-speed isochronous endpoint: eight
/// transaction slots, one per microframe, whose status words the controller rewrites (bit 31 = Active until the slot
/// ran, bits 30:28 = errors, 27:16 = bytes moved for an IN slot). The endpoint, direction, packet size and the pages of
/// the data buffer (`region`, page aligned, `pages` pages, physically contiguous) never change, so they are written
/// once here; `itd_arm` fills the slots for each lap. All slots start inactive.
pub fn itd_init(itd: usize, target: &Target, endpoint: u8, max_packet: u16, input: bool, region: u64, pages: usize) {
    for word in 0..ITD_BYTES / 4 {
        write32(itd + 4 * word, 0);
    }
    write32(itd, TERMINATE);
    let page = |n: usize| if n < pages { ((region as u32) & !0xfff).wrapping_add(n as u32 * 0x1000) } else { 0 };
    write32(itd + 4 * 9, page(0) | (u32::from(endpoint & 0xf) << 8) | u32::from(target.address));
    write32(itd + 4 * 10, page(1) | (u32::from(input) << 11) | u32::from(max_packet & 0x7ff));
    write32(itd + 4 * 11, page(2) | 1); // Mult 1: one transaction per microframe
    for n in 3..7 {
        write32(itd + 4 * (9 + n), page(n));
    }
}

/// Arms an iTD for its next visit: transaction `j` runs in microframe `j * step` and moves `lengths[j]` bytes at offset
/// `j * max_packet` of the buffer region. The last transaction raises an interrupt (IOC) when it completes.
pub fn itd_arm(itd: usize, step: usize, lengths: &[u16], max_packet: u16) {
    for slot in 0..8 {
        write32(itd + 4 * (1 + slot), 0);
    }
    for (j, &length) in lengths.iter().enumerate() {
        let offset = j * usize::from(max_packet);
        let ioc = u32::from(j + 1 == lengths.len());
        let word = (1 << 31) | (u32::from(length) << 16) | (ioc << 15) | (((offset >> 12) as u32) << 12) | (offset & 0xfff) as u32;
        write32(itd + 4 * (1 + j * step), word);
    }
}

/// True while any of the iTD's `count` transactions (every `step` microframes) has not run yet.
pub fn itd_active(itd: usize, step: usize, count: usize) -> bool {
    (0..count).any(|j| read32(itd + 4 * (1 + j * step)) & (1 << 31) != 0)
}

/// The finished transaction `j`: (bytes moved, error bits). The error bits are data buffer error, babble, transaction error.
pub fn itd_result(itd: usize, step: usize, j: usize) -> (usize, u32) {
    let word = read32(itd + 4 * (1 + j * step));
    (((word >> 16) & 0xfff) as usize, (word >> 28) & 0x7)
}

/// Interrupts this driver claimed: the INTx self-test counts these, not the vector's raw count (a floating pin delivers
/// interrupts of its own).
static CLAIMS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The completion interrupt: acknowledge what is ours (only bits that were enabled, since the line may be shared) and
/// let the audio code service its iTD rings.
fn isr(context: usize) {
    let Some(controller) = controller(context) else { return };
    let enabled = read32(controller.operational + OP_USBINTR);
    let status = read32(controller.operational + OP_USBSTS) & enabled & (STS_USBINT | STS_USBERRINT | STS_IAA | STS_HSE);
    if status == 0 {
        return;
    }
    write32(controller.operational + OP_USBSTS, status);
    crate::msi::claim();
    CLAIMS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    if status & STS_HSE != 0 {
        let command = read32(controller.operational + OP_USBCMD);
        controller.fault = Some((status, command, controller.frame_now()));
    }
    if status & (STS_USBINT | STS_USBERRINT) != 0 {
        crate::usb_audio::ehci_event(context, controller.frame_now());
    }
}

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

/// Capabilities word of an interrupt QH: start-split in microframe 0, complete-splits in microframes 2-4 when a hub
/// translates (a low/full-speed device); a high-speed endpoint just polls in microframe 0.
fn interrupt_capabilities(target: &Target) -> u32 {
    let masks = if target.hub_address != 0 { 0x01 | (0x1c << 8) } else { 0x01 };
    qh_capabilities(target) | masks
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

/// A hub found on a root port (one level is supported): kept so that its ports can be asked about later, because nothing
/// tells the host when a device behind a hub comes or goes (a root port has a status-change bit; a hub's port is only
/// visible by asking the hub).
#[derive(Clone, Copy)]
struct Hub {
    root_port: u8,
    target: Target,
    ports: u8,
    /// Bit n: the device on hub port n was addressed by us and is still believed to be there.
    claimed: u16,
    power_good_ticks: u64,
}

const MAX_HUBS: usize = 4;

#[derive(Clone, Copy)]
#[allow(dead_code)] // endpoint and packet size describe the pipe
struct KeyboardPipe {
    target: Target,
    endpoint: u8,
    max_packet: u16,
    toggle: bool,
    /// The root port this keyboard is on, or that its hub is on: what `claimed_ports` records.
    root_port: u8,
}

pub struct Controller {
    pub pci: pci::Device,
    operational: usize,
    pub ports: u8,
    frame: usize,
    /// The periodic frame list (one page, 1024 links): keyboards' interrupt queue heads hang off every entry.
    periodic: usize,
    next_address: u8,
    /// What enumeration was doing when it last failed (for the boot log).
    pub step: &'static str,
    /// The data toggle the controller left in the last completed qTD of the previous chain.
    last_toggle: bool,
    /// How long `wait_chain` waits for the current transfer, in ticks (`TIMEOUT_TICKS` unless a caller shortens it).
    wait_limit: u64,
    /// Isochronous iTD rings linked into the periodic list: 0 = playback, 1 = capture (`ISO_RING` iTDs each).
    iso: [Option<usize>; 2],
    /// The I/O APIC vector the controller's INTx line was routed to (0 = none) and whether probing already failed.
    irq_vector: u8,
    irq_failed: bool,
    /// Host system error seen by the interrupt (the controller halts itself on one): (USBSTS, USBCMD, frame) at that moment.
    fault: Option<(u32, u32, usize)>,
    /// Root ports where a keyboard was found (bit n = port n); rescans skip them.
    pub claimed_ports: u16,
    hubs: [Option<Hub>; MAX_HUBS],
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

        // Low memory (below 256 MiB), like v1: the first real EHCI (a Sandy Bridge PCH) halted with a host
        // system error on a frame near 3 GiB; the cause is not understood, so stay where v1 worked.
        let frame = memory::allocate_contiguous(1, Some(256 << 20)).ok_or(Error::NoMemory)? as usize;
        let periodic = memory::allocate_contiguous(1, Some(256 << 20)).ok_or(Error::NoMemory)? as usize;
        for entry in 0..PERIODIC_ENTRIES {
            unsafe { (periodic as *mut u32).add(entry).write_volatile(TERMINATE) };
        }
        if hccparams & 1 != 0 {
            write32(operational + OP_CTRLDSSEGMENT, 0); // 64-bit capable: all our DMA memory is below 4 GiB
        }
        // The async list starts as the control QH alone, pointing at itself and marked head of the
        // reclamation list (H bit). Keyboard QHs are linked in only when a keyboard is added
        // (`relink`), so the controller never walks an empty one; real silicon is stricter than QEMU.
        let qh_at = |index: usize| (frame + if index == 0 { OFFSET_CONTROL_QH } else { OFFSET_KEYBOARD_QH + (index - 1) * 0x40 }) as u32;
        for index in 0..=MAX_KEYBOARDS {
            let words = qh_at(index) as *mut u32;
            unsafe {
                // Keyboard queue heads live on the periodic list, not the async one: a split interrupt endpoint
                // on the async list halted the Sandy Bridge PCH with a host system error.
                words.write_volatile(if index == 0 { qh_at(0) | LINK_QH } else { TERMINATE });
                words.add(1).write_volatile(if index == 0 { 1 << 15 } else { 0 });
                words.add(2).write_volatile(1 << 30);
                words.add(4).write_volatile(TERMINATE);
                words.add(5).write_volatile(TERMINATE);
            }
        }
        write32(operational + OP_ASYNCLISTADDR, qh_at(0));
        write32(operational + OP_PERIODICLISTBASE, periodic as u32);
        write32(operational + OP_FRINDEX, 0);

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
        Ok(Controller { pci: device, operational, ports, frame, periodic, next_address: 1, step: "", last_toggle: false, wait_limit: TIMEOUT_TICKS, iso: [None; 2], irq_vector: 0, irq_failed: false, fault: None, claimed_ports: 0, hubs: [None; MAX_HUBS], keyboards: [None; MAX_KEYBOARDS] })
    }

    fn port_register(&self, port: u8) -> usize {
        self.operational + OP_PORTSC + 4 * (usize::from(port) - 1)
    }

    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    /// The port's connect-status-change bit: set by the controller whenever a device is plugged in or removed and cleared
    /// only by software (enumeration clears it). A port we enumerated earlier with this bit set again has seen its device
    /// come or go since, even if a device is connected now (an unplug and replug in between leaves no other trace).
    pub fn port_connect_changed(&self, port: u8) -> bool {
        self.port_status(port) & (1 << 1) != 0
    }

    pub fn port_has_device(&self, port: u8) -> bool {
        let status = self.port_status(port);
        status & PORT_CONNECTED != 0 && status & PORT_OWNER == 0
    }

    pub fn usbsts(&self) -> u32 {
        read32(self.operational + OP_USBSTS)
    }

    fn set_async(&self, on: bool) -> bool {
        let command = read32(self.operational + OP_USBCMD);
        write32(self.operational + OP_USBCMD, if on { command | CMD_ASYNC_ENABLE } else { command & !CMD_ASYNC_ENABLE });
        wait_until(5, || (read32(self.operational + OP_USBSTS) & STS_ASYNC_RUNNING != 0) == on)
    }

    fn set_periodic(&self, on: bool) -> bool {
        let command = read32(self.operational + OP_USBCMD);
        write32(self.operational + OP_USBCMD, if on { command | CMD_PERIODIC_ENABLE } else { command & !CMD_PERIODIC_ENABLE });
        wait_until(5, || (read32(self.operational + OP_USBSTS) & STS_PERIODIC_RUNNING != 0) == on)
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
            if apic::ticks().wrapping_sub(start) >= self.wait_limit {
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

    /// `bulk_transfer` that gives up after `ticks` (10 ms each) instead of a second: for polling an IN endpoint that is
    /// usually idle (a USB serial adapter). A timeout means "nothing there", and the queue is left clean.
    pub fn bulk_transfer_within(&mut self, ticks: u64, target: &Target, endpoint: u8, max_packet: u16, buffer: u32, length: usize, data_in: bool, toggle: &mut bool) -> Result<usize, Error> {
        self.wait_limit = ticks;
        let outcome = self.bulk_transfer(target, endpoint, max_packet, buffer, length, data_in, toggle);
        self.wait_limit = TIMEOUT_TICKS;
        outcome
    }

    pub fn get_descriptor(&mut self, target: &Target, kind: u8, index: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(target, 0x80, 0x06, (u16::from(kind) << 8) | u16::from(index), 0, buffer)
    }

    pub fn set_configuration(&mut self, target: &Target, value: u8) -> Result<(), Error> {
        self.control_transfer(target, 0x00, 0x09, u16::from(value), 0, &mut []).map(|_| ())
    }

    /// Waits until the port has shown a device continuously for `stable_ms` (checked every 10 ms), for at most `timeout_ms`.
    /// A device that was just plugged in bounces for a moment, and a USB 3 stick on a USB 2 port may drop off the bus and
    /// come back while it falls back from SuperSpeed (seen on a Kingston DataTraveler 3.0 on an nForce EHCI port): a reset
    /// during a bounce finds nothing. Other systems debounce the same way (100 ms of stable connection, up to 2 s).
    pub fn wait_port_stable(&self, port: u8, stable_ms: u32, timeout_ms: u32) -> bool {
        let (mut stable, mut waited) = (0, 0);
        while waited < timeout_ms {
            if self.port_has_device(port) {
                stable += 10;
                if stable >= stable_ms {
                    return true;
                }
            } else {
                stable = 0;
            }
            apic::wait_ticks(1);
            waited += 10;
        }
        false
    }

    /// Resets a root port with a device on it and leaves a high-speed device enabled
    /// at address 0. Anything slower is released to the companion controller.
    fn reset_root_port(&mut self, port: u8) -> Result<(), Error> {
        let register = self.port_register(port);
        let status = read32(register);
        if status & PORT_CONNECTED == 0 || status & PORT_OWNER != 0 {
            return Err(Error::NoDevice);
        }
        if !self.wait_port_stable(port, 100, 2000) {
            return Err(Error::NoDevice); // it did not hold still long enough to be worth resetting
        }
        let status = read32(register);
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
        apic::wait_ticks(6); // reset recovery (the standard asks for 10 ms; flash sticks coming out of a USB 3 fallback want more)
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
        self.step = "read the first 8 bytes of the device descriptor at address 0";
        // A device fresh out of reset may not answer the first request (it is still starting up): ask a few times, 50 ms
        // apart, before giving the port up for a full reset (the same is done by other systems' enumeration code).
        let mut tries = 0;
        loop {
            match self.get_descriptor(&target, 1, 0, &mut first) {
                Ok(_) => break,
                Err(error) if tries >= 3 => return Err(error),
                Err(_) => {
                    tries += 1;
                    apic::wait_ticks(5);
                }
            }
        }
        target.max_packet0 = u16::from(first[7]).max(8);
        let address = self.allocate_address().ok_or(Error::Unsupported)?;
        self.step = "SET_ADDRESS";
        self.control_transfer(&target, 0x00, 0x05, u16::from(address), 0, &mut [])?;
        apic::wait_ticks(1); // SET_ADDRESS recovery time
        target.address = address;
        let mut descriptor = [0u8; 18];
        self.step = "read the full device descriptor at the new address";
        self.get_descriptor(&target, 1, 0, &mut descriptor)?;
        Ok(Found { target, descriptor })
    }

    /// Resets a root port and addresses whatever answers, looking through one level
    /// of hub. Fills `found` with the devices (never the hub itself); returns how many.
    pub fn enumerate_port(&mut self, port: u8, found: &mut [Option<Found>; 8]) -> Result<usize, Error> {
        self.step = "reset the root port";
        self.reset_root_port(port)?;
        apic::wait_ticks(10); // let the device (or the hub's own reset) settle before the first transfer
        let device = self.address_default_device(Speed::High, 0, 0)?;
        if device.descriptor[4] != 9 {
            found[0] = Some(device);
            return Ok(1);
        }
        // A hub (on Sandy Bridge, the rate-matching hub): configure it and bring up its ports.
        let hub = device.target;
        self.step = "configure the hub";
        self.set_configuration(&hub, 1)?;
        self.step = "read the hub descriptor";
        let mut hub_descriptor = [0u8; 9];
        self.control_transfer(&hub, 0xa0, 0x06, 0x2900, 0, &mut hub_descriptor)?;
        let hub_ports = hub_descriptor[2].min(8);
        let power_good_ticks = u64::from(hub_descriptor[5]).max(10) * 2 / 10 + 1; // bPwrOn2PwrGood is in 2 ms units
        crate::println!("USB: EHCI port {port}: a hub with {hub_ports} port(s), vendor {:04x} product {:04x}.", u16::from_le_bytes([device.descriptor[8], device.descriptor[9]]), u16::from_le_bytes([device.descriptor[10], device.descriptor[11]]));
        let mut count = 0;
        let mut claimed = 0u16;
        for hub_port in 1..=hub_ports {
            match self.hub_bring_up_port(&hub, hub_port, power_good_ticks) {
                Ok(speed) => {
                    if let Ok(child) = self.address_default_device(speed, hub.address, hub_port) {
                        claimed |= 1 << hub_port;
                        if count < found.len() {
                            found[count] = Some(child);
                            count += 1;
                        }
                    }
                }
                Err(error) => {
                    // "look for a device" failing just means the port is empty; anything later is worth saying.
                    if self.step != "look for a device on a hub port" {
                        crate::println!("USB: EHCI hub port {port}.{hub_port}: bring-up failed ({error:?} at \"{}\").", self.step);
                    }
                    continue;
                }
            }
        }
        self.forget_hub(port);
        if let Some(slot) = self.hubs.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(Hub { root_port: port, target: hub, ports: hub_ports, claimed, power_good_ticks });
        }
        Ok(count)
    }

    /// The hub on root `port`, if one was enumerated there: its USB address, for marking the disks behind it stale.
    pub fn hub_address_on_port(&self, port: u8) -> Option<u8> {
        self.hubs.iter().flatten().find(|hub| hub.root_port == port).map(|hub| hub.target.address)
    }

    /// Drops what is known about the hub on root `port` (the port's device went away, or is about to be enumerated again).
    pub fn forget_hub(&mut self, port: u8) {
        for slot in self.hubs.iter_mut() {
            if slot.is_some_and(|hub| hub.root_port == port) {
                *slot = None;
            }
        }
    }

    /// Makes `rescan_hub` treat the device on a hub port as new: its port is reset and the device addressed again.
    pub fn unclaim_hub_child(&mut self, hub_address: u8, hub_port: u8) {
        for hub in self.hubs.iter_mut().flatten() {
            if hub.target.address == hub_address {
                hub.claimed &= !(1 << hub_port);
            }
        }
    }

    /// A hub port's status and change words (`GET_STATUS`, class request to the port).
    fn hub_port_status(&mut self, hub: &Target, port: u8) -> Result<(u16, u16), Error> {
        let mut raw = [0u8; 4];
        self.control_transfer(hub, 0xa3, 0x00, 0, u16::from(port), &mut raw)?;
        Ok((u16::from_le_bytes([raw[0], raw[1]]), u16::from_le_bytes([raw[2], raw[3]])))
    }

    /// Whether the device behind `hub_address`'s port `hub_port` has gone: its root port has lost the hub, the hub no longer
    /// answers, or the hub says nothing is connected. A disk behind a hub gets no removal signal from the controller, and a
    /// transfer to a missing device takes a long time to give up, so the storage driver asks first.
    pub fn hub_child_vanished(&mut self, hub_address: u8, hub_port: u8) -> bool {
        let Some(hub) = self.hubs.iter().flatten().find(|hub| hub.target.address == hub_address).copied() else { return true };
        if !self.port_has_device(hub.root_port) || self.port_connect_changed(hub.root_port) {
            return true;
        }
        self.hub_port_status(&hub.target, hub_port).map_or(true, |(state, _)| state & 1 == 0)
    }

    /// Looks at every port of the hub on root `port`: a device that went away or was replaced (the port's connection-change
    /// bit, which we cleared after addressing it) is reported in the returned mask of hub ports, and a device on a port we
    /// have no claim on is brought up and put in `found`. Returns `(departed mask, found count)`.
    pub fn rescan_hub(&mut self, port: u8, found: &mut [Option<Found>; 8]) -> (u16, usize) {
        let Some(mut hub) = self.hubs.iter().flatten().find(|hub| hub.root_port == port).copied() else { return (0, 0) };
        const C_PORT_CONNECTION: u16 = 16;
        let (mut departed, mut count) = (0u16, 0usize);
        for hub_port in 1..=hub.ports {
            let Ok((state, change)) = self.hub_port_status(&hub.target, hub_port) else { break };
            let claimed = hub.claimed & (1 << hub_port) != 0;
            if claimed && (state & 1 == 0 || change & 1 != 0) {
                departed |= 1 << hub_port;
                hub.claimed &= !(1 << hub_port);
                let _ = self.control_transfer(&hub.target, 0x23, 0x01, C_PORT_CONNECTION, u16::from(hub_port), &mut []);
            }
            if hub.claimed & (1 << hub_port) == 0 && state & 1 != 0 {
                match self.hub_bring_up_port(&hub.target, hub_port, hub.power_good_ticks) {
                    Ok(speed) => match self.address_default_device(speed, hub.target.address, hub_port) {
                        Ok(child) => {
                            hub.claimed |= 1 << hub_port;
                            if count < found.len() {
                                found[count] = Some(child);
                                count += 1;
                            }
                        }
                        Err(error) => crate::println!("USB: EHCI hub port {port}.{hub_port}: a device is there but would not address ({error:?} at \"{}\").", self.step),
                    },
                    Err(error) => crate::println!("USB: EHCI hub port {port}.{hub_port}: a device is there but its port would not reset ({error:?}; hub status {state:#06x})."),
                }
            }
        }
        if let Some(slot) = self.hubs.iter_mut().flatten().find(|slot| slot.root_port == port) {
            *slot = hub;
        }
        (departed, count)
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
        self.step = "power a hub port";
        self.control_transfer(hub, 0x23, 0x03, PORT_POWER_FEATURE, port_index, &mut [])?;
        apic::wait_ticks(power_good_ticks.max(10));
        self.step = "look for a device on a hub port";
        let (state, change) = status(self)?;
        if state & 1 == 0 {
            if state & !0x0100 != 0 || change != 0 {
                crate::println!("USB: EHCI hub port {port}: nothing connected, but status {state:#06x} change {change:#06x}.");
            }
            return Err(Error::NoDevice);
        }
        self.step = "reset a hub port";
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

    /// Rebuilds the periodic list. Frame `f` visits, in this order: the playback iTD `f % ISO_RING`, the capture iTD
    /// `f % ISO_RING` (isochronous descriptors must come first in a frame's chain), then every keyboard QH in use. The
    /// schedule is switched off while the links change.
    fn relink(&mut self) {
        self.set_periodic(false);
        let mut head = TERMINATE;
        for index in (0..MAX_KEYBOARDS).rev() {
            if self.keyboards[index].is_some() {
                let qh = self.keyboard_qh(index);
                unsafe { (qh as *mut u32).write_volatile(head) };
                head = qh as u32 | LINK_QH;
            }
        }
        // An iTD ring is visited every ISO_RING frames, so the chain behind iTD k depends only on k.
        let mut chain = [head; ISO_RING];
        for (k, link) in chain.iter_mut().enumerate() {
            for which in [1usize, 0] {
                if let Some(base) = self.iso[which] {
                    let itd = base + k * ITD_BYTES;
                    write32(itd, *link); // type 0 = iTD, so the address is the link
                    *link = itd as u32;
                }
            }
        }
        for entry in 0..PERIODIC_ENTRIES {
            unsafe { (self.periodic as *mut u32).add(entry).write_volatile(chain[entry % ISO_RING]) };
        }
        if head != TERMINATE || self.iso.iter().any(Option::is_some) {
            self.set_periodic(true);
        }
    }

    // ---- isochronous transfers ----

    /// The frame (millisecond) the controller is in, as an index into the periodic list.
    pub fn frame_now(&self) -> usize {
        ((read32(self.operational + OP_FRINDEX) >> 3) as usize) & (PERIODIC_ENTRIES - 1)
    }

    /// Links a ring of `ISO_RING` iTDs (at `itds`, 64 bytes each, built by `itd_init`) into every frame: slot 0 is
    /// playback, 1 is capture.
    pub fn iso_install(&mut self, which: usize, itds: usize) {
        self.iso[which] = Some(itds);
        self.relink();
    }

    /// Unlinks a ring. When this returns the schedule has been stopped and restarted without it, so the controller no
    /// longer touches the iTDs and the caller may free them.
    pub fn iso_remove(&mut self, which: usize) {
        self.iso[which] = None;
        self.relink();
    }

    /// Routes the controller's INTx line to a vector and turns on the completion interrupts (USBINT, USBERRINT) that
    /// the iTDs' IOC bits raise. Probed once (EHCI has no MSI here and the ACPI routing table is not read): each
    /// candidate pin gets a self-test with the async-advance doorbell, which makes the controller raise an interrupt on
    /// purpose. Returns false if no pin delivered it; the caller then services the rings by polling.
    pub fn enable_interrupts(&mut self, index: usize) -> bool {
        if self.irq_vector == 0 {
            if self.irq_failed {
                return false;
            }
            let (first, end) = crate::ioapic::gsi_range();
            let candidates = crate::msi::intx_candidates(self.pci.interrupt_line());
            for (position, &gsi) in candidates.iter().enumerate() {
                if gsi == u32::MAX || gsi == 0 || gsi < first || gsi >= end || candidates[..position].contains(&gsi) {
                    continue;
                }
                let Some(vector) = crate::msi::attach_intx(gsi, isr, index) else { continue };
                if self.interrupt_self_test(|| CLAIMS.load(core::sync::atomic::Ordering::Relaxed)) {
                    self.irq_vector = vector;
                    crate::println!("EHCI {:02x}:{:02x}.{}: interrupts on I/O APIC pin {gsi}, vector {vector:#x}.", self.pci.bus, self.pci.device, self.pci.function);
                    break;
                }
                crate::msi::detach_intx(vector, index);
            }
            if self.irq_vector == 0 {
                self.irq_failed = true;
                return false;
            }
        }
        write32(self.operational + OP_USBSTS, STS_USBINT | STS_USBERRINT);
        write32(self.operational + OP_USBINTR, STS_USBINT | STS_USBERRINT | STS_HSE);
        true
    }

    /// Stops the completion interrupts (the route stays, for the next stream).
    pub fn disable_interrupts(&mut self) {
        write32(self.operational + OP_USBINTR, 0);
    }

    pub fn status_word(&self) -> u32 {
        read32(self.operational + OP_USBSTS)
    }

    pub fn interrupts_enabled(&self) -> bool {
        self.irq_vector != 0 && read32(self.operational + OP_USBINTR) != 0
    }

    /// Makes the controller raise "interrupt on async advance" and waits for the vector's count to move.
    fn interrupt_self_test(&mut self, count: impl Fn() -> u64) -> bool {
        let before = count();
        write32(self.operational + OP_USBSTS, STS_IAA);
        write32(self.operational + OP_USBINTR, STS_IAA);
        let running = self.set_async(true);
        if running {
            write32(self.operational + OP_USBCMD, read32(self.operational + OP_USBCMD) | CMD_IAAD);
        }
        let delivered = running && wait_until(5, || count() != before);
        write32(self.operational + OP_USBINTR, 0);
        self.set_async(false);
        write32(self.operational + OP_USBSTS, STS_IAA);
        delivered
    }

    /// Dedicates a queue head to a keyboard's interrupt-IN endpoint and arms it.
    /// Returns the keyboard's index on this controller.
    pub fn add_keyboard(&mut self, target: Target, endpoint: u8, max_packet: u16, root_port: u8) -> Result<usize, Error> {
        let index = self.keyboards.iter().position(Option::is_none).ok_or(Error::Unsupported)?;
        self.keyboards[index] = Some(KeyboardPipe { target, endpoint, max_packet, toggle: false, root_port });
        self.relink();
        unsafe {
            let words = self.keyboard_qh(index) as *mut u32;
            words.add(1).write_volatile(qh_characteristics(&target, endpoint, max_packet, false, false));
            words.add(2).write_volatile(interrupt_capabilities(&target));
        }
        self.arm_keyboard(index);
        Ok(index)
    }

    /// Takes the keyboards behind root `port` off the async list (the device was unplugged, or unplugged and plugged back in)
    /// and frees their entries for the next enumeration. Returns a bit mask of the removed keyboard indexes. `relink` stops
    /// the schedule while it rewrites the links, so the controller never walks a queue head that is being dropped.
    pub fn remove_keyboards_on_port(&mut self, port: u8) -> u8 {
        let mut removed = 0u8;
        for index in 0..MAX_KEYBOARDS {
            if self.keyboards[index].is_some_and(|pipe| pipe.root_port == port) {
                self.keyboards[index] = None;
                removed |= 1 << index;
            }
        }
        if removed != 0 {
            self.relink();
        }
        removed
    }

    /// Takes the keyboards behind hub port `hub_port` of hub `hub_address` off the async list (that device was unplugged or
    /// replaced). Returns a bit mask of the removed keyboard indexes.
    pub fn remove_keyboards_behind(&mut self, hub_address: u8, hub_port: u8) -> u8 {
        let mut removed = 0u8;
        for index in 0..MAX_KEYBOARDS {
            if self.keyboards[index].is_some_and(|pipe| pipe.target.hub_address == hub_address && pipe.target.hub_port == hub_port) {
                self.keyboards[index] = None;
                removed |= 1 << index;
            }
        }
        if removed != 0 {
            self.relink();
        }
        removed
    }

    /// Queues one IN transfer of a report on keyboard `index`'s queue head.
    fn arm_keyboard(&mut self, index: usize) {
        let Some(pipe) = self.keyboards[index] else { return };
        self.set_periodic(false);
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
        self.set_periodic(true);
    }

    /// One line per keyboard queue for `usb`: the controller registers and the report qTD's token (0x80 = still active, i.e.
    /// the controller has not completed the transfer), so a keyboard that never reports can be told from a dead controller.
    pub fn describe_keyboards(&self, print: &impl Fn(core::fmt::Arguments)) {
        let (cmd, sts, list) = (read32(self.operational + OP_USBCMD), read32(self.operational + OP_USBSTS), read32(self.operational + OP_ASYNCLISTADDR));
        print(format_args!("    USBCMD {cmd:#x} USBSTS {sts:#x} ASYNCLISTADDR {list:#x} frame {:#x}", self.frame));
        print(format_args!("    FRINDEX {:#x} PERIODICLISTBASE {:#x} USBINTR {:#x} iso {:x?}", read32(self.operational + OP_FRINDEX), read32(self.operational + OP_PERIODICLISTBASE), read32(self.operational + OP_USBINTR), self.iso));
        if let Some((status, command, frame)) = self.fault {
            print(format_args!("    HOST SYSTEM ERROR seen: USBSTS {status:#x} USBCMD {command:#x} in frame {frame}"));
        }
        let control = unsafe { core::slice::from_raw_parts((self.frame + OFFSET_CONTROL_QH) as *const u32, 12) };
        print(format_args!("    control QH {:#x}: {:x?}", self.frame + OFFSET_CONTROL_QH, control));
        for index in 0..MAX_KEYBOARDS {
            if self.keyboards[index].is_none() {
                continue;
            }
            let token = unsafe { ((self.keyboard_qtd(index) + 8) as *const u32).read_volatile() };
            let qh = unsafe { core::slice::from_raw_parts(self.keyboard_qh(index) as *const u32, 12) };
            print(format_args!("    keyboard {index}: qTD token {token:#x}, QH {:#x}: {:x?}", self.keyboard_qh(index), qh));
        }
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
