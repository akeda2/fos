//! A polling xHCI (USB 3.x host controller) driver, built for keyboards: it finds
//! devices on the root ports, addresses them, runs control transfers on their default
//! pipe and keeps one interrupt-IN endpoint per keyboard armed. (v1's `xhci.rs` is the
//! mass-storage version; the storage milestone M4 will extend this one.)
//!
//! xHCI in one paragraph. The controller reads *rings* of 16-byte Transfer Request
//! Blocks (TRBs) from RAM: a command ring and one transfer ring per endpoint (software
//! produces), and an event ring (the controller produces). Each TRB has a *cycle bit*
//! that software flips each time it wraps a ring, so neither side needs a separate
//! "valid" flag; a link TRB at the end of a ring points back to its start. After
//! queueing TRBs software rings a *doorbell* register. Per-device state is a *device
//! context* the controller owns; software changes it by filling an *input context* and
//! issuing Address Device / Configure Endpoint. The Device Context Base Address Array
//! (DCBAA) lists every device context.
//!
//! Scope: root-port devices only (no hubs yet), polling only, one event-ring segment,
//! 4 KiB host pages. All memory comes from the physical allocator below 4 GiB, so a
//! 32-bit-DMA controller and a 64-bit one behave the same.

use crate::{memory, paging, pci, timer};

pub const MAX_CONTROLLERS: usize = 4;
pub const MAX_SLOTS: usize = 16;
const RING_TRBS: usize = 256;
/// The event ring is four frames: a high-speed isochronous stream completes a transfer every 125 us
/// (8000 events/s), and the foreground only drains the ring between other work.
const EVENT_TRBS: usize = 1024;
const TRB_BYTES: u64 = 16;
const DCI_COUNT: usize = 32;
const TIMEOUT_TICKS: u64 = 100; // 1 s

const CAP_HCSPARAMS1: usize = 0x04;
const CAP_HCSPARAMS2: usize = 0x08;
const CAP_HCCPARAMS1: usize = 0x10;
const CAP_DBOFF: usize = 0x14;
const CAP_RTSOFF: usize = 0x18;
const OP_USBCMD: usize = 0x00;
const OP_USBSTS: usize = 0x04;
const OP_PAGESIZE: usize = 0x08;
const OP_CRCR: usize = 0x18;
/// CRCR bits: Command Abort (write 1), and Command Ring Running (read-only, 1 while the controller is working through the ring).
const CRCR_CA: u64 = 1 << 2;
const CRCR_CRR: u32 = 1 << 3;
const OP_DCBAAP: usize = 0x30;
const OP_CONFIG: usize = 0x38;
const OP_PORTS: usize = 0x400;

const CMD_RUN: u32 = 1;
const CMD_RESET: u32 = 2;
const STS_HALTED: u32 = 1;
const STS_NOT_READY: u32 = 1 << 11;

const PORTSC_CONNECTED: u32 = 1 << 0;
const PORTSC_ENABLED: u32 = 1 << 1;
const PORTSC_RESET: u32 = 1 << 4;
const PORTSC_POWER: u32 = 1 << 9;
const PORTSC_PRC: u32 = 1 << 21;
const PORTSC_CHANGE_BITS: u32 = 0x7f << 17;
/// Bits of PORTSC that read back as state and must be written back unchanged (the
/// rest are write-1-to-clear change flags or read-only).
const PORTSC_PRESERVE: u32 = (1 << 0) | (1 << 3) | (0xf << 10) | (1 << 30) | (0xf << 5) | (1 << 9) | (0x3 << 14) | (0x7 << 25);

const TRB_NORMAL: u32 = 1;
const TRB_ISOCH: u32 = 5;
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
const TRB_STOP_ENDPOINT: u32 = 15;
const TRB_NOOP_COMMAND: u32 = 23;
const TRB_TRANSFER_EVENT: u32 = 32;
const TRB_COMMAND_COMPLETION: u32 = 33;
const TRB_CYCLE: u32 = 1;
const TRB_TOGGLE_CYCLE: u32 = 2;
const TRB_IOC: u32 = 1 << 5;
const TRB_IMMEDIATE_DATA: u32 = 1 << 6;
const TRB_ISP: u32 = 1 << 2;
const TRB_DIR_IN: u32 = 1 << 16;
const CC_SUCCESS: u8 = 1;
const CC_SHORT_PACKET: u8 = 13;

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // payloads are shown through Debug
pub enum Error {
    Timeout,
    Command(u8),
    Transfer(u8),
    NoDevice,
    NoMemory,
    Unsupported,
}

#[derive(Clone, Copy)]
pub struct Trb {
    pub parameter: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    fn kind(&self) -> u32 {
        (self.control >> 10) & 0x3f
    }
    fn code(&self) -> u8 {
        (self.status >> 24) as u8
    }
    fn slot(&self) -> u8 {
        (self.control >> 24) as u8
    }
    fn endpoint(&self) -> u32 {
        (self.control >> 16) & 0x1f
    }
}

/// Queues one isochronous transfer (one service interval's worth of data) on `ring`, without ringing the
/// doorbell. "Start isochronous ASAP" (SIA) lets the controller choose the frame, each transfer following the
/// previous one; every transfer asks for an event (IOC), so the stream handler learns its actual length.
/// Returns the TRB's address, which names its completion event.
pub fn push_isoc(ring: &mut Ring, buffer: u64, length: u32) -> u64 {
    ring.push(buffer, length, (TRB_ISOCH << 10) | TRB_IOC | TRB_ISP | (1 << 31))
}

/// Rings a doorbell register (`Controller::doorbell_address`) for endpoint `dci`.
pub fn ring_doorbell_at(address: usize, dci: u32) {
    write32(address, dci);
}

/// A producer ring: one frame of TRBs whose last entry links back to the start.
#[derive(Clone, Copy)]
pub struct Ring {
    base: u64,
    enqueue: usize,
    cycle: bool,
}

const NO_RING: Ring = Ring { base: 0, enqueue: 0, cycle: true };

fn frame() -> Option<u64> {
    memory::allocate_contiguous(1, Some(1 << 32))
}

fn write_trb(address: u64, trb: &Trb) {
    unsafe {
        let pointer = address as *mut u32;
        pointer.write_volatile(trb.parameter as u32);
        pointer.add(1).write_volatile((trb.parameter >> 32) as u32);
        pointer.add(2).write_volatile(trb.status);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        // The cycle bit is in the last dword: written last, so a half-written TRB is
        // never mistaken for a valid one.
        pointer.add(3).write_volatile(trb.control);
    }
}

fn read_trb(address: u64) -> Trb {
    unsafe {
        let pointer = address as *const u32;
        let control = pointer.add(3).read_volatile();
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        Trb { parameter: u64::from(pointer.read_volatile()) | u64::from(pointer.add(1).read_volatile()) << 32, status: pointer.add(2).read_volatile(), control }
    }
}

impl Ring {
    pub fn allocate() -> Option<Ring> {
        let base = frame()?;
        write_trb(base + (RING_TRBS as u64 - 1) * TRB_BYTES, &Trb { parameter: base, status: 0, control: TRB_LINK << 10 | TRB_TOGGLE_CYCLE });
        Some(Ring { base, enqueue: 0, cycle: true })
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    fn is_allocated(&self) -> bool {
        self.base != 0
    }

    fn enqueue_address(&self) -> u64 {
        self.base + self.enqueue as u64 * TRB_BYTES
    }

    /// Queues one TRB (`control` without its cycle bit); returns its address, which is
    /// how its completion event names it.
    pub fn push(&mut self, parameter: u64, status: u32, control: u32) -> u64 {
        let at = self.enqueue_address();
        write_trb(at, &Trb { parameter, status, control: control | u32::from(self.cycle) });
        self.enqueue += 1;
        if self.enqueue == RING_TRBS - 1 {
            let link = self.base + (RING_TRBS as u64 - 1) * TRB_BYTES;
            write_trb(link, &Trb { parameter: self.base, status: 0, control: TRB_LINK << 10 | TRB_TOGGLE_CYCLE | u32::from(self.cycle) });
            self.cycle = !self.cycle;
            self.enqueue = 0;
        }
        at
    }
}

fn read32(address: usize) -> u32 {
    unsafe { (address as *const u32).read_volatile() }
}
fn write32(address: usize, value: u32) {
    unsafe { (address as *mut u32).write_volatile(value) }
}
/// 64-bit registers are written as two 32-bit accesses, low half first: not every
/// controller accepts a 64-bit MMIO write.
fn write64(address: usize, value: u64) {
    write32(address, value as u32);
    write32(address + 4, (value >> 32) as u32);
}

fn wait_until(ticks: u64, mut condition: impl FnMut() -> bool) -> bool {
    let start = timer::ticks();
    loop {
        #[cfg(target_arch = "x86_64")]
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

#[derive(Clone, Copy)]
struct Slot {
    used: bool,
    port: u8,
    /// Route string: the hub port taken at each hub tier below the root port, one nibble
    /// per tier, first tier in the lowest nibble (0 for a device on a root port).
    route: u32,
    speed: u8,
    output: u64,
    input: u64,
    rings: [Ring; DCI_COUNT],
}

const EMPTY_SLOT: Slot = Slot { used: false, port: 0, route: 0, speed: 0, output: 0, input: 0, rings: [NO_RING; DCI_COUNT] };

/// A device with a slot and an address.
#[derive(Clone, Copy)]
pub struct Device {
    pub slot: u8,
    /// The root port the device (or the chain of hubs above it) hangs off.
    pub port: u8,
    /// Hub ports taken below the root port (see `Slot::route`) and how many hubs sit
    /// between the device and the root port.
    pub route: u32,
    pub tier: u8,
    /// For a low/full-speed device behind a high-speed hub: that hub's slot and the port
    /// it is reached through, the transaction translator the controller must use.
    pub tt: Option<(u8, u8)>,
    /// PORTSC speed ID: 1 full, 2 low, 3 high, 4 SuperSpeed, 5 SuperSpeed+.
    pub speed: u8,
    pub control_max_packet: u16,
}

impl Device {
    /// "3" for a root port, "3.2" for port 2 of a hub on root port 3, and so on.
    pub fn path(&self) -> Path {
        Path { port: self.port, route: self.route, tier: self.tier }
    }
}

pub struct Path {
    port: u8,
    route: u32,
    tier: u8,
}

impl core::fmt::Display for Path {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}", self.port)?;
        for tier in 0..self.tier {
            write!(formatter, ".{}", (self.route >> (4 * u32::from(tier))) & 0xf)?;
        }
        Ok(())
    }
}

pub struct Controller {
    pub pci: pci::Device,
    #[allow(dead_code)]
    mmio: usize,
    operational: usize,
    runtime: usize,
    doorbells: usize,
    pub ports: u8,
    /// Context records are 64 bytes instead of 32 when HCCPARAMS1.CSZ is set.
    context_bytes: u64,
    /// Position in `CONTROLLERS`: events, the soft queue and the interrupt handler are keyed by it.
    pub index: usize,
    /// True once MSI delivers this controller's events (otherwise the foreground drains the ring).
    pub interrupt_driven: bool,
    /// Whether the interrupt path in use is MSI-X (otherwise plain MSI).
    pub uses_msix: bool,
    dcbaa: u64,
    command_ring: Ring,
    slots: [Slot; MAX_SLOTS + 1],
}

static mut CONTROLLERS: [Option<Controller>; MAX_CONTROLLERS] = [const { None }; MAX_CONTROLLERS];
static mut COUNT: usize = 0;

/// Where this controller's event ring stands, kept outside `Controller` because the interrupt handler
/// consumes the ring too and must never alias the `&mut Controller` the foreground holds.
#[derive(Clone, Copy)]
struct EventState {
    base: u64,
    index: usize,
    cycle: bool,
    /// Addresses of interrupter 0's ERDP and IMAN registers and of USBSTS.
    erdp: usize,
    iman: usize,
    usbsts: usize,
    ready: bool,
}

static mut EVENTS: [EventState; MAX_CONTROLLERS] = [EventState { base: 0, index: 0, cycle: true, erdp: 0, iman: 0, usbsts: 0, ready: false }; MAX_CONTROLLERS];

/// Events nobody handled inline (command completions, port changes, a keyboard's report, the transfers
/// the foreground waits for), handed from whoever drained the ring to the foreground. One producer at a
/// time (the interrupt handler, or the foreground with interrupts off) and one consumer: a plain
/// head/tail ring is enough.
const SOFT_CAPACITY: usize = 512;

struct SoftQueue {
    items: [Trb; SOFT_CAPACITY],
    head: core::sync::atomic::AtomicUsize,
    tail: core::sync::atomic::AtomicUsize,
    dropped: core::sync::atomic::AtomicU64,
}

static mut SOFT: [SoftQueue; MAX_CONTROLLERS] = [const {
    SoftQueue { items: [Trb { parameter: 0, status: 0, control: 0 }; SOFT_CAPACITY], head: core::sync::atomic::AtomicUsize::new(0), tail: core::sync::atomic::AtomicUsize::new(0), dropped: core::sync::atomic::AtomicU64::new(0) }
}; MAX_CONTROLLERS];

fn soft_push(index: usize, trb: Trb) {
    use core::sync::atomic::Ordering;
    let queue = unsafe { &mut (*core::ptr::addr_of_mut!(SOFT))[index] };
    let head = queue.head.load(Ordering::Relaxed);
    let next = (head + 1) % SOFT_CAPACITY;
    if next == queue.tail.load(Ordering::Acquire) {
        queue.dropped.fetch_add(1, Ordering::Relaxed);
        return;
    }
    queue.items[head] = trb;
    queue.head.store(next, Ordering::Release);
}

fn soft_pop(index: usize) -> Option<Trb> {
    use core::sync::atomic::Ordering;
    let queue = unsafe { &mut (*core::ptr::addr_of_mut!(SOFT))[index] };
    let tail = queue.tail.load(Ordering::Relaxed);
    if tail == queue.head.load(Ordering::Acquire) {
        return None;
    }
    let trb = queue.items[tail];
    queue.tail.store((tail + 1) % SOFT_CAPACITY, Ordering::Release);
    Some(trb)
}

/// Consumes every event the controller has written. Audio transfer events are handled on the spot
/// (`usb_audio::irq_event`: refill a packet, copy captured bytes) so audio never depends on the
/// foreground; everything else goes to the soft queue. Called from the interrupt handler, or by the
/// foreground with interrupts disabled: never from both at once.
fn drain(index: usize) {
    let state = unsafe { &mut (*core::ptr::addr_of_mut!(EVENTS))[index] };
    if !state.ready {
        return;
    }
    let mut consumed = false;
    let mut taken = 0;
    loop {
        taken += 1;
        if taken > 16 * EVENT_TRBS {
            crate::println!("xHCI {index}: the event ring never ran dry ({taken} events in one pass); giving up this pass.");
            break;
        }
        let at = state.base + state.index as u64 * TRB_BYTES;
        let trb = read_trb(at);
        if (trb.control & TRB_CYCLE != 0) != state.cycle {
            break;
        }
        state.index += 1;
        if state.index == EVENT_TRBS {
            state.index = 0;
            state.cycle = !state.cycle;
        }
        consumed = true;
        #[cfg(target_arch = "x86_64")]
        if crate::usb_audio::irq_event(index, &trb) {
            continue;
        }
        soft_push(index, trb);
    }
    if consumed {
        let dequeue = state.base + state.index as u64 * TRB_BYTES;
        write64(state.erdp, dequeue | (1 << 3)); // bit 3: clear "event handler busy"
    }
}

/// The MSI handler: acknowledge the interrupt (IMAN.IP and USBSTS.EINT are write-1-to-clear), then drain.
fn interrupt(index: usize) {
    let state = unsafe { (*core::ptr::addr_of!(EVENTS))[index] };
    if !state.ready {
        return;
    }
    // A controller that keeps raising the interrupt (the vector outranks the timer's, so the machine would freeze with
    // the timer starved) is switched back to polling: more than 2000 interrupts without one timer tick is not an event
    // stream. The foreground drains the ring itself from then on.
    static mut LAST_TICK: u64 = 0;
    static mut BURST: u32 = 0;
    let now = timer::ticks();
    unsafe {
        if now != LAST_TICK {
            LAST_TICK = now;
            BURST = 0;
        }
        BURST += 1;
        if BURST > 2000 {
            let usbcmd = state.usbsts - 4; // USBCMD sits just before USBSTS in the operational registers
            write32(usbcmd, read32(usbcmd) & !(1 << 2)); // INTE off
            write32(state.iman, 0b01); // clear IP, IE off
            write32(state.usbsts, 1 << 3);
            let burst = BURST;
            crate::println!("xHCI {index}: interrupt storm ({burst} in one tick); events are polled from now on.");
            BURST = 0;
            return;
        }
    }
    write32(state.iman, 0b11); // clear IP, keep IE
    write32(state.usbsts, 1 << 3); // clear EINT
    drain(index);
}

/// Switches controllers from foreground draining to MSI (call after the settings file is read: `no-msi`
/// decides). Each controller needs the MSI capability and must pass a self-test (a no-op command whose
/// completion must raise the interrupt); interrupt moderation is set to 1 ms so a high-speed isochronous
/// stream's 8,000 completions per second cost about 1,000 interrupts. A controller that fails stays in
/// the polled mode, where the foreground drains the ring itself as before.
pub fn enable_interrupts_all(no_msi: bool) {
    for index in 0..count() {
        let Some(controller) = controller(index) else { continue };
        if no_msi {
            crate::println!("xHCI {:02x}:{:02x}.{}: interrupts polled (settings: no-msi).", controller.pci.bus, controller.pci.device, controller.pci.function);
            continue;
        }
        // The NEC uPD720200 on clock1 (1033:0194) froze the machine at the first doorbell write after MSI was enabled while the
        // boot stick was attached to it (an old kernel enabled MSI fine with no disk on it; not understood). A controller that
        // carries a disk stays polled: the foreground drains its ring, as it did before MSI.
        #[cfg(target_arch = "x86_64")]
        if crate::usb_storage::uses_xhci(index) && controller.pci.vendor == 0x1033 {
            crate::println!("xHCI {:02x}:{:02x}.{}: a USB disk is on this NEC controller; events stay polled (MSI froze it).", controller.pci.bus, controller.pci.device, controller.pci.function);
            continue;
        }
        match controller.enable_interrupts() {
            Ok(vector) => crate::println!("xHCI {:02x}:{:02x}.{}: {} on vector {vector:#x} (the self-test interrupt arrived); events no longer need the foreground.", controller.pci.bus, controller.pci.device, controller.pci.function, if controller.uses_msix { "MSI-X" } else { "MSI" }),
            Err(reason) => crate::println!("xHCI {:02x}:{:02x}.{}: events polled ({reason}).", controller.pci.bus, controller.pci.device, controller.pci.function),
        }
    }
}

pub fn count() -> usize {
    unsafe { COUNT }
}

pub fn controller(index: usize) -> Option<&'static mut Controller> {
    unsafe { (*core::ptr::addr_of_mut!(CONTROLLERS)).get_mut(index)?.as_mut() }
}

pub fn speed_name(speed: u8) -> &'static str {
    match speed {
        1 => "full speed",
        2 => "low speed",
        3 => "high speed",
        4 => "SuperSpeed",
        5 => "SuperSpeed+",
        _ => "unknown speed",
    }
}

/// Finds and starts every xHCI controller (PCI class 0x0c, subclass 0x03, interface
/// 0x30). Returns how many came up.
pub fn init_all() -> usize {
    for device in pci::devices().filter(|d| d.class == 0x0c && d.subclass == 0x03 && d.interface == 0x30) {
        if count() == MAX_CONTROLLERS {
            break;
        }
        match Controller::init(device, count()) {
            Ok(controller) => unsafe {
                CONTROLLERS[COUNT] = Some(controller);
                COUNT += 1;
            },
            Err(error) => crate::println!("xHCI at {:02x}:{:02x}.{}: initialisation failed ({error:?}).", device.bus, device.device, device.function),
        }
    }
    count()
}

/// Hands the controller from the firmware (which may be emulating a keyboard through
/// SMM) to the OS: set the OS-owned bit in the legacy-support capability, wait for
/// the BIOS-owned bit to clear, and switch off the firmware's SMI sources.
fn take_ownership_from_firmware(mmio: usize, hccparams1: u32) {
    let mut offset = ((hccparams1 >> 16) * 4) as usize;
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
            let control = read32(mmio + offset + 4);
            const SMI_ENABLES: u32 = (0x3 << 1) | (0xff << 5) | (0x7 << 17);
            write32(mmio + offset + 4, (control & !SMI_ENABLES) | (0x7 << 29));
            return;
        }
        let next = ((header >> 8) & 0xff) as usize;
        if next == 0 {
            return;
        }
        offset += next * 4;
    }
}

impl Controller {
    fn init(device: pci::Device, index: usize) -> Result<Controller, Error> {
        device.force_power_state_d0();
        device.enable_memory_and_bus_master();
        let (base, size) = device.memory_bar(0).ok_or(Error::NoDevice)?;
        paging::map_device(base, size);
        let mmio = base as usize;
        let cap_length = unsafe { (mmio as *const u8).read_volatile() } as usize;
        let hcsparams1 = read32(mmio + CAP_HCSPARAMS1);
        let hcsparams2 = read32(mmio + CAP_HCSPARAMS2);
        let hccparams1 = read32(mmio + CAP_HCCPARAMS1);
        let operational = mmio + cap_length;
        let doorbells = mmio + (read32(mmio + CAP_DBOFF) & !3) as usize;
        let runtime = mmio + (read32(mmio + CAP_RTSOFF) & !0x1f) as usize;
        let max_slots = hcsparams1 & 0xff;
        let ports = ((hcsparams1 >> 24) & 0xff) as u8;
        if max_slots == 0 || ports == 0 || ports > 31 {
            return Err(Error::Unsupported);
        }
        take_ownership_from_firmware(mmio, hccparams1);

        // Halt (if running), then reset: HCRST restores every register's power-on value.
        if !wait_until(100, || read32(operational + OP_USBSTS) & STS_NOT_READY == 0) {
            return Err(Error::Timeout);
        }
        let command = read32(operational + OP_USBCMD);
        if command & CMD_RUN != 0 {
            write32(operational + OP_USBCMD, command & !CMD_RUN);
            if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED != 0) {
                return Err(Error::Timeout);
            }
        }
        write32(operational + OP_USBCMD, CMD_RESET);
        if !wait_until(100, || read32(operational + OP_USBCMD) & CMD_RESET == 0 && read32(operational + OP_USBSTS) & STS_NOT_READY == 0) {
            return Err(Error::Timeout);
        }
        if read32(operational + OP_PAGESIZE) & 1 == 0 {
            return Err(Error::Unsupported); // only 4 KiB host pages
        }
        write32(operational + OP_CONFIG, max_slots.min(MAX_SLOTS as u32));

        // DCBAA: entry 0 points at the scratchpad buffer array (pages the controller
        // wants for itself), entries 1.. at each device's output context.
        let dcbaa = frame().ok_or(Error::NoMemory)?;
        let scratchpads = (((hcsparams2 >> 21) & 0x1f) << 5) | ((hcsparams2 >> 27) & 0x1f);
        if scratchpads != 0 {
            let array = memory::allocate_contiguous((u64::from(scratchpads) * 8).div_ceil(memory::FRAME), Some(1 << 32)).ok_or(Error::NoMemory)?;
            for index in 0..u64::from(scratchpads) {
                let page = frame().ok_or(Error::NoMemory)?;
                unsafe { (array as *mut u64).add(index as usize).write_volatile(page) };
            }
            unsafe { (dcbaa as *mut u64).write_volatile(array) };
        }
        write64(operational + OP_DCBAAP, dcbaa);

        let command_ring = Ring::allocate().ok_or(Error::NoMemory)?;
        write64(operational + OP_CRCR, command_ring.base | 1);

        // Event ring (interrupter 0): a one-entry segment table. ERSTSZ and ERDP are
        // written before ERSTBA, whose write arms the ring.
        let event_ring = memory::allocate_contiguous((EVENT_TRBS * TRB_BYTES as usize / 4096) as u64, Some(1 << 32)).ok_or(Error::NoMemory)?;
        let segment_table = frame().ok_or(Error::NoMemory)?;
        unsafe {
            let entry = segment_table as *mut u32;
            entry.write_volatile(event_ring as u32);
            entry.add(1).write_volatile((event_ring >> 32) as u32);
            entry.add(2).write_volatile(EVENT_TRBS as u32);
        }
        let interrupter = runtime + 0x20;
        write32(interrupter + 0x08, 1);
        write64(interrupter + 0x18, event_ring);
        write64(interrupter + 0x10, segment_table);

        let mut controller = Controller {
            pci: device,
            mmio,
            operational,
            runtime,
            doorbells,
            ports,
            context_bytes: if hccparams1 & (1 << 2) != 0 { 64 } else { 32 },
            index,
            interrupt_driven: false,
            uses_msix: false,
            dcbaa,
            command_ring,
            slots: [EMPTY_SLOT; MAX_SLOTS + 1],
        };
        unsafe {
            (*core::ptr::addr_of_mut!(EVENTS))[index] = EventState { base: event_ring, index: 0, cycle: true, erdp: interrupter + 0x18, iman: interrupter, usbsts: operational + OP_USBSTS, ready: true };
        }
        write32(operational + OP_USBCMD, CMD_RUN);
        if !wait_until(100, || read32(operational + OP_USBSTS) & STS_HALTED == 0) {
            return Err(Error::Timeout);
        }
        controller.command(0, 0, TRB_NOOP_COMMAND << 10)?; // the command ring must work
        for port in 1..=ports {
            let status = read32(controller.port_register(port));
            if status & PORTSC_POWER == 0 {
                write32(controller.port_register(port), (status & PORTSC_PRESERVE) | PORTSC_POWER);
            }
        }
        // The reset dropped every link; give SuperSpeed devices time to train.
        timer::wait_ticks(15);
        Ok(controller)
    }

    fn port_register(&self, port: u8) -> usize {
        self.operational + OP_PORTS + (usize::from(port) - 1) * 0x10
    }

    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    /// The port's connect-status-change bit (PORTSC bit 17): set by the controller whenever a device is plugged in or removed,
    /// cleared only by software (enumerating the port clears it). Set again on a port we already enumerated, it says the
    /// device came or went since, even if something is connected now (an unplug and replug leaves no other trace).
    pub fn port_connect_changed(&self, port: u8) -> bool {
        self.port_status(port) & (1 << 17) != 0
    }

    pub fn slot_in_use_for_port(&self, port: u8) -> bool {
        self.slots[1..].iter().any(|slot| slot.used && slot.port == port)
    }

    /// The root port and route string of a slot that is in use (`None` if the slot is free): lets a driver find the devices
    /// it keeps of its own (the HID keyboards) that sit on a port about to be released.
    pub fn slot_location(&self, slot: u8) -> Option<(u8, u32)> {
        let entry = self.slots.get(usize::from(slot))?;
        entry.used.then_some((entry.port, entry.route))
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        write32(self.doorbells + usize::from(slot) * 4, target);
    }

    /// The next event for the foreground, if any: from the soft queue, else by draining the ring now
    /// (with interrupts off, so the interrupt handler cannot drain at the same moment).
    pub fn next_event(&mut self) -> Option<Trb> {
        if let Some(trb) = soft_pop(self.index) {
            return Some(trb);
        }
        let was = crate::cpu::interrupts_save_and_disable();
        drain(self.index);
        crate::cpu::interrupts_restore(was);
        soft_pop(self.index)
    }

    /// Switches this controller's events to MSI, falling back to MSI-X (table entry 0 = interrupter 0) if
    /// the plain-MSI self-test fails or the capability is missing (QEMU's xHCI has only MSI-X; the NEC
    /// uPD720200 on the Sandy Bridge machine has both but only MSI-X works, as Linux's xhci also tries
    /// it first). Returns the vector, or why the controller stays polled.
    pub fn enable_interrupts(&mut self) -> Result<u8, &'static str> {
        let msi = self.pci.find_capability(0x05).is_some();
        let msix = self.pci.find_capability(0x11).is_some();
        if !msi && !msix {
            return Err("no MSI or MSI-X capability");
        }
        let mut reason = "no interrupt path passed its self-test";
        for use_msix in [false, true] {
            if (use_msix && !msix) || (!use_msix && !msi) {
                continue;
            }
            let Some(vector) = crate::msi::allocate(interrupt, self.index) else { return Err("no free MSI vector") };
            let enabled = if use_msix { crate::msi::enable_msix(&self.pci, vector) } else { crate::msi::enable_msi(&self.pci, vector) };
            if !enabled {
                crate::msi::release(vector);
                reason = "could not enable MSI";
                continue;
            }
            let interrupter = self.runtime + 0x20;
            write32(interrupter + 0x04, 4000); // IMOD: at most one interrupt per 4000 x 250 ns = 1 ms
            write32(interrupter, 0b11); // IMAN: clear a pending bit, enable the interrupter
            write32(self.operational + OP_USBCMD, read32(self.operational + OP_USBCMD) | (1 << 2)); // USBCMD.INTE
            let before = crate::msi::count(vector);
            // Queue the no-op and wait for the interrupt WITHOUT draining the event ring ourselves: some
            // controllers (the NEC uPD720200) drop a moderated interrupt if the ring is emptied before the
            // moderation timer expires. Once the interrupt is in (or the wait times out) the handler or
            // `wait_event` below consumes the completion.
            let command = self.command_ring.push(0, 0, TRB_NOOP_COMMAND << 10);
            self.ring_doorbell(0, 0);
            let start = timer::ticks();
            while crate::msi::count(vector) == before && timer::ticks().wrapping_sub(start) < 5 {
                core::hint::spin_loop();
            }
            if self.wait_event(TIMEOUT_TICKS, |event| event.kind() == TRB_COMMAND_COMPLETION && event.parameter == command).is_err() {
                return Err("the self-test command failed");
            }
            if crate::msi::count(vector) != before {
                self.interrupt_driven = true;
                self.uses_msix = use_msix;
                return Ok(vector);
            }
            write32(self.operational + OP_USBCMD, read32(self.operational + OP_USBCMD) & !(1 << 2));
            write32(interrupter, 1); // IMAN: IE off
            if use_msix {
                self.pci.disable_msix();
            } else {
                self.pci.disable_msi();
            }
            crate::msi::release(vector);
            reason = if use_msix { "the MSI-X self-test interrupt never arrived" } else { "the MSI self-test interrupt never arrived" };
        }
        Err(reason)
    }

    /// The doorbell register address for `slot`, for code (the audio stream handlers) that rings it
    /// without holding the controller.
    pub fn doorbell_address(&self, slot: u8) -> usize {
        self.doorbells + usize::from(slot) * 4
    }

    /// Waits for an event accepted by `wanted`. Anything else (port changes, reports
    /// from keyboards already running) goes to the background handler.
    fn wait_event(&mut self, ticks: u64, wanted: impl Fn(&Trb) -> bool) -> Result<Trb, Error> {
        let start = timer::ticks();
        loop {
            #[cfg(target_arch = "x86_64")]
        crate::debug::pet();
            while let Some(event) = self.next_event() {
                if wanted(&event) {
                    return Ok(event);
                }
                crate::usb_hid::background_event(self, &event);
            }
            if timer::ticks().wrapping_sub(start) >= ticks {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    fn command(&mut self, parameter: u64, status: u32, control: u32) -> Result<Trb, Error> {
        // Address Device sends SET_ADDRESS to the device, and a controller keeps retrying a
        // silent device for up to ~5 s before it reports failure: give it longer than that,
        // or the next command queues behind a command we already gave up on.
        let ticks = if control >> 10 & 0x3f == TRB_ADDRESS_DEVICE { 700 } else { TIMEOUT_TICKS };
        let cycle = self.command_ring.cycle;
        let at = self.command_ring.push(parameter, status, control);
        self.ring_doorbell(0, 0);
        let event = match self.wait_event(ticks, |event| event.kind() == TRB_COMMAND_COMPLETION && event.parameter == at) {
            Ok(event) => event,
            Err(error) => {
                if matches!(error, Error::Timeout) {
                    self.abort_command_ring(at, cycle);
                }
                return Err(error);
            }
        };
        match event.code() {
            CC_SUCCESS => Ok(event),
            code => Err(Error::Command(code)),
        }
    }

    /// A command that never completed (Address Device to a device behind a hub that does not route, say) keeps the
    /// controller busy on it, and every command queued behind it waits too: without this the whole controller stopped
    /// answering after one such timeout (on the Skylake it could no longer read the boot disk). The sequence of xHCI
    /// 4.6.1.2: write CRCR.CA and wait for CRCR.CRR to clear (the controller stops, with its dequeue pointer still on the
    /// dead command, and posts a Command Ring Stopped event and a Command Aborted completion). The CRCR pointer can only
    /// be set to a 64-byte boundary, and TRBs are 16 bytes, so the ring is not moved: as Linux does, the dead TRB (`at`,
    /// written with `cycle`) is rewritten as a No-Op with the same cycle bit and the doorbell is rung, so the controller
    /// skips it and carries on with whatever is queued behind.
    fn abort_command_ring(&mut self, at: u64, cycle: bool) {
        let crcr = self.operational + OP_CRCR;
        // The abort bit only means something while the ring is running; written to a stopped ring it can stay pending
        // and abort the next command (QEMU does exactly that).
        if read32(crcr) & CRCR_CRR != 0 {
            write64(crcr, CRCR_CA);
        }
        if !wait_until(50, || read32(crcr) & CRCR_CRR == 0) {
            crate::println!("USB: xHCI {:02x}:{:02x}.{}: a command timed out and the command ring did not stop when aborted.", self.pci.bus, self.pci.device, self.pci.function);
            return;
        }
        write_trb(at, &Trb { parameter: 0, status: 0, control: TRB_NOOP_COMMAND << 10 | u32::from(cycle) });
        self.ring_doorbell(0, 0);
        // The Command Ring Stopped, Command Aborted and No-Op completion events: take them off the event ring (anything
        // else goes to the background handler).
        let start = timer::ticks();
        while timer::ticks().wrapping_sub(start) < 3 {
            while let Some(event) = self.next_event() {
                crate::usb_hid::background_event(self, &event);
            }
        }
        crate::println!("USB: xHCI {:02x}:{:02x}.{}: a command timed out; the command ring was aborted and restarted.", self.pci.bus, self.pci.device, self.pci.function);
    }

    /// `usb abort-test`: queues a command with the wrong cycle bit (the controller sees an empty ring and never answers),
    /// lets it time out, aborts, and then runs a real command (No-Op) to prove the ring carries on behind the dead TRB.
    /// QEMU never leaves a command hanging, so this is the only way to run `abort_command_ring`'s pointer arithmetic there.
    pub fn abort_self_test(&mut self) -> bool {
        let cycle = self.command_ring.cycle;
        self.command_ring.cycle = !cycle;
        let dead = self.command_ring.push(0, 0, TRB_NOOP_COMMAND << 10);
        self.command_ring.cycle = cycle;
        self.ring_doorbell(0, 0);
        let answered = self.wait_event(TIMEOUT_TICKS, |event| event.kind() == TRB_COMMAND_COMPLETION && event.parameter == dead).is_ok();
        self.abort_command_ring(dead, cycle);
        !answered && self.command(0, 0, TRB_NOOP_COMMAND << 10).is_ok() && self.command(0, 0, TRB_NOOP_COMMAND << 10).is_ok()
    }

    fn context_slot(&self, context: u64) -> u64 {
        context + self.context_bytes
    }

    fn context_endpoint(&self, context: u64, dci: u32) -> u64 {
        context + self.context_bytes * (1 + u64::from(dci))
    }

    /// Resets a root port by hand (`usb try`): a hot reset (PR), or for a SuperSpeed port a warm reset (WPR), which also
    /// retrains the link from scratch. Returns whether the reset completed.
    pub fn reset_port_raw(&mut self, port: u8, warm: bool) -> bool {
        let register = self.port_register(port);
        let (request, done_bit) = if warm { (1u32 << 31, 1u32 << 19) } else { (PORTSC_RESET, PORTSC_PRC) };
        write32(register, (read32(register) & PORTSC_PRESERVE) | request);
        let done = wait_until(100, || read32(register) & done_bit != 0);
        write32(register, (read32(register) & PORTSC_PRESERVE) | PORTSC_CHANGE_BITS);
        timer::wait_ticks(3);
        done
    }

    /// Resets a connected root port, gives the device a slot and an address.
    pub fn open_port(&mut self, port: u8) -> Result<Device, Error> {
        if port == 0 || port > self.ports {
            return Err(Error::NoDevice);
        }
        let register = self.port_register(port);
        let status = read32(register);
        if status & PORTSC_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        write32(register, (status & PORTSC_PRESERVE) | PORTSC_CHANGE_BITS);
        let speed_now = (status >> 10) & 0xf;
        let trained = status & PORTSC_ENABLED != 0 && (status >> 5) & 0xf == 0;
        if !(speed_now >= 4 && trained) {
            // USB 2 devices need a port reset; a SuperSpeed port that already trained its link is left alone.
            write32(register, (read32(register) & PORTSC_PRESERVE) | PORTSC_RESET);
            if !wait_until(100, || read32(register) & PORTSC_PRC != 0) {
                return Err(Error::NoDevice);
            }
            write32(register, (read32(register) & PORTSC_PRESERVE) | PORTSC_CHANGE_BITS);
            timer::wait_ticks(3); // 10 ms reset recovery
        }
        let status = read32(register);
        if status & PORTSC_ENABLED == 0 || status & PORTSC_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        let speed = ((status >> 10) & 0xf) as u8;
        let max_packet: u16 = match speed {
            // Low and full speed devices may have an 8-byte default pipe; 64 would corrupt the
            // first transfer. Evaluate Context fixes it once bMaxPacketSize0 is known.
            1 | 2 => 8,
            3 => 64,
            _ => 512,
        };
        let event = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        let slot = event.slot();
        if slot == 0 || usize::from(slot) > MAX_SLOTS {
            return Err(Error::Unsupported);
        }
        let mut result = self.address_device(slot, port, speed, max_packet, 0, 0, None);
        if result.is_err() && speed >= 4 {
            // A SuperSpeed port whose link trained by itself is left alone, which works for most devices; some (an internal
            // device on the Latitude E7270's second USB 3 port) do not answer SET_ADDRESS until the port has been reset.
            self.release_slot(slot);
            if !self.reset_port_raw(port, false) || read32(register) & PORTSC_ENABLED == 0 {
                return Err(Error::NoDevice);
            }
            let event = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
            let slot = event.slot();
            if slot == 0 || usize::from(slot) > MAX_SLOTS {
                return Err(Error::Unsupported);
            }
            result = self.address_device(slot, port, speed, max_packet, 0, 0, None);
            if result.is_err() {
                self.release_slot(slot);
            } else {
                timer::wait_ticks(1);
            }
            return result;
        }
        if result.is_err() {
            self.release_slot(slot);
        } else {
            timer::wait_ticks(1); // SET_ADDRESS recovery time, with some margin, before the first transfer
        }
        result
    }

    /// Gives a device behind a hub a slot and an address. `hub` is the hub's own device,
    /// `hub_port` the port (1-based) the device sits on and `speed` the PORTSC speed ID
    /// the hub reported for it (the hub, not the root port, knows).
    pub fn open_hub_port(&mut self, hub: &Device, hub_port: u8, speed: u8) -> Result<Device, Error> {
        if hub.tier >= 4 {
            return Err(Error::Unsupported); // the route string holds five tiers; keep one in reserve
        }
        let route = hub.route | u32::from(hub_port) << (4 * u32::from(hub.tier));
        let tt = if speed >= 3 {
            None
        } else if hub.speed == 3 {
            Some((hub.slot, hub_port))
        } else {
            hub.tt
        };
        let max_packet: u16 = match speed {
            1 | 2 => 8,
            3 => 64,
            _ => 512,
        };
        let event = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        let slot = event.slot();
        if slot == 0 || usize::from(slot) > MAX_SLOTS {
            return Err(Error::Unsupported);
        }
        let result = self.address_device(slot, hub.port, speed, max_packet, route, hub.tier + 1, tt);
        if result.is_err() {
            self.release_slot(slot);
        } else {
            timer::wait_ticks(1);
        }
        result
    }

    /// Marks a device's slot as a hub (Configure Endpoint with only the slot context
    /// changed): the controller needs the hub flag and port count to route split
    /// transactions through its transaction translator. Done after SET_CONFIGURATION.
    pub fn configure_hub(&mut self, device: &Device, ports: u8, think_time: u8) -> Result<(), Error> {
        let (input, output) = (self.slots[usize::from(device.slot)].input, self.slots[usize::from(device.slot)].output);
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            (input as *mut u32).add(1).write_volatile(1); // A0: the slot context
            core::ptr::copy_nonoverlapping(output as *const u8, self.context_slot(input) as *mut u8, 32);
            let slot_context = self.context_slot(input) as *mut u32;
            slot_context.write_volatile(slot_context.read_volatile() | (1 << 26)); // Hub
            let word1 = slot_context.add(1).read_volatile() & 0x00ff_ffff;
            slot_context.add(1).write_volatile(word1 | u32::from(ports) << 24); // number of ports
            let word2 = slot_context.add(2).read_volatile() & !(3 << 16);
            slot_context.add(2).write_volatile(word2 | u32::from(think_time & 3) << 16); // TT think time
        }
        self.command(input, 0, (TRB_CONFIGURE_ENDPOINT << 10) | (u32::from(device.slot) << 24)).map(|_| ())
    }

    pub fn slot_in_use_for_route(&self, port: u8, route: u32) -> bool {
        self.slots[1..].iter().any(|slot| slot.used && slot.port == port && slot.route == route)
    }

    #[allow(clippy::too_many_arguments)]
    fn address_device(&mut self, slot: u8, port: u8, speed: u8, max_packet: u16, route: u32, tier: u8, tt: Option<(u8, u8)>) -> Result<Device, Error> {
        let input = frame().ok_or(Error::NoMemory)?;
        let output = frame().ok_or(Error::NoMemory)?;
        let control_ring = Ring::allocate().ok_or(Error::NoMemory)?;
        let entry = &mut self.slots[usize::from(slot)];
        *entry = EMPTY_SLOT;
        entry.used = true;
        entry.port = port;
        entry.route = route;
        entry.speed = speed;
        entry.input = input;
        entry.output = output;
        entry.rings[1] = control_ring;
        unsafe { (self.dcbaa as *mut u64).add(usize::from(slot)).write_volatile(output) };
        unsafe {
            // Input context: add the slot context (A0) and the default pipe (A1).
            (input as *mut u32).add(1).write_volatile(0b11);
            let slot_context = self.context_slot(input) as *mut u32;
            slot_context.write_volatile((1 << 27) | (u32::from(speed) << 20) | route); // one context entry, speed, route string
            slot_context.add(1).write_volatile(u32::from(port) << 16); // root hub port number
            if let Some((hub_slot, hub_port)) = tt {
                slot_context.add(2).write_volatile(u32::from(hub_slot) | u32::from(hub_port) << 8); // transaction translator
            }
            let endpoint = self.context_endpoint(input, 1) as *mut u32;
            endpoint.add(1).write_volatile((3 << 1) | (4 << 3) | (u32::from(max_packet) << 16)); // CErr 3, control, max packet
            endpoint.add(2).write_volatile(control_ring.base as u32 | 1); // dequeue pointer + cycle state
            endpoint.add(3).write_volatile((control_ring.base >> 32) as u32);
            endpoint.add(4).write_volatile(8);
        }
        self.command(input, 0, (TRB_ADDRESS_DEVICE << 10) | (u32::from(slot) << 24))?;
        Ok(Device { slot, port, route, tier, tt, speed, control_max_packet: max_packet })
    }

    /// Frees every slot this driver holds for `port` (a failed enumeration must not leave the
    /// device counted as "in use").
    pub fn release_port_slots(&mut self, port: u8) {
        for slot in 1..=MAX_SLOTS as u8 {
            if self.slots[usize::from(slot)].used && self.slots[usize::from(slot)].port == port {
                self.release_slot(slot);
            }
        }
    }

    /// Frees the slot of the device at exactly this route below `port` (a failed hub-port
    /// enumeration must not leave it counted as "in use"; the hub itself stays).
    pub fn release_route_slots(&mut self, port: u8, route: u32) {
        for slot in 1..=MAX_SLOTS as u8 {
            let entry = &self.slots[usize::from(slot)];
            if entry.used && entry.port == port && entry.route == route {
                self.release_slot(slot);
            }
        }
    }

    /// USBSTS (bit 2 = host system error, bit 12 = host controller error) for diagnostics.
    pub fn usbsts(&self) -> u32 {
        read32(self.operational + OP_USBSTS)
    }

    pub fn release_slot(&mut self, slot: u8) {
        let _ = self.command(0, 0, (TRB_DISABLE_SLOT << 10) | (u32::from(slot) << 24));
        let entry = &mut self.slots[usize::from(slot)];
        if entry.used {
            for ring in entry.rings.iter().filter(|ring| ring.is_allocated()) {
                memory::free(ring.base, 1);
            }
            memory::free(entry.input, 1);
            memory::free(entry.output, 1);
            unsafe { (self.dcbaa as *mut u64).add(usize::from(slot)).write_volatile(0) };
        }
        *entry = EMPTY_SLOT;
    }

    /// Tells the controller the default pipe's real max packet size when the device
    /// descriptor says it differs from the guess made at address time.
    pub fn set_control_max_packet(&mut self, device: &mut Device, max_packet: u16) -> Result<(), Error> {
        if max_packet == device.control_max_packet {
            return Ok(());
        }
        let input = self.slots[usize::from(device.slot)].input;
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            (input as *mut u32).add(1).write_volatile(0b10); // A1: the default pipe only
            let endpoint = self.context_endpoint(input, 1) as *mut u32;
            endpoint.add(1).write_volatile((3 << 1) | (4 << 3) | (u32::from(max_packet) << 16));
        }
        self.command(input, 0, (TRB_EVALUATE_CONTEXT << 10) | (u32::from(device.slot) << 24))?;
        device.control_max_packet = max_packet;
        Ok(())
    }

    /// One control transfer on the default pipe: SETUP, an optional DATA stage through
    /// a bounce frame (so the caller's buffer may sit anywhere), STATUS. Returns the
    /// bytes the data stage moved.
    pub fn control_transfer(&mut self, device: &Device, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8]) -> Result<usize, Error> {
        let data_in = request_type & 0x80 != 0;
        if data.len() > 4096 {
            return Err(Error::Unsupported);
        }
        let bounce = frame().ok_or(Error::NoMemory)?;
        let result = self.control_transfer_inner(device, request_type, request, value, index, data, data_in, bounce);
        memory::free(bounce, 1);
        if let Err(Error::Transfer(_)) = result {
            self.reset_endpoint(device, 1); // a stall halts the default pipe; un-halt it
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn control_transfer_inner(&mut self, device: &Device, request_type: u8, request: u8, value: u16, index: u16, data: &mut [u8], data_in: bool, bounce: u64) -> Result<usize, Error> {
        let length = data.len() as u32;
        if !data_in && length != 0 {
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), bounce as *mut u8, data.len()) };
        }
        let setup = u64::from(request_type) | u64::from(request) << 8 | u64::from(value) << 16 | u64::from(index) << 32 | u64::from(length) << 48;
        let transfer_type: u32 = match (length, data_in) {
            (0, _) => 0,
            (_, true) => 3,
            (_, false) => 2,
        };
        let ring = &mut self.slots[usize::from(device.slot)].rings[1];
        ring.push(setup, 8, (TRB_SETUP << 10) | TRB_IMMEDIATE_DATA | (transfer_type << 16));
        if length != 0 {
            ring.push(bounce, length, (TRB_DATA << 10) | TRB_ISP | if data_in { TRB_DIR_IN } else { 0 });
        }
        let status_in = !(length != 0 && data_in);
        let last = ring.push(0, 0, (TRB_STATUS << 10) | TRB_IOC | if status_in { TRB_DIR_IN } else { 0 });
        self.ring_doorbell(device.slot, 1);
        let mut residual_bytes = 0u32;
        loop {
            let slot = device.slot;
            let event = self.wait_event(TIMEOUT_TICKS, |event| event.kind() == TRB_TRANSFER_EVENT && event.slot() == slot && event.endpoint() == 1)?;
            match event.code() {
                CC_SUCCESS if event.parameter == last => break,
                CC_SHORT_PACKET => {
                    residual_bytes = event.status & 0x00ff_ffff; // a short data stage is fine; the status stage still follows
                    if event.parameter == last {
                        break;
                    }
                }
                CC_SUCCESS => {}
                code => return Err(Error::Transfer(code)),
            }
        }
        let transferred = (length - residual_bytes) as usize;
        if data_in && length != 0 {
            let copy = transferred.min(data.len());
            unsafe { core::ptr::copy_nonoverlapping(bounce as *const u8, data.as_mut_ptr(), copy) };
        }
        Ok(transferred)
    }

    /// After a stall or error: un-halts the endpoint and skips whatever was left on its
    /// ring (Reset Endpoint, then Set TR Dequeue Pointer).
    pub fn reset_endpoint(&mut self, device: &Device, dci: u32) {
        if dci == 0 || dci >= DCI_COUNT as u32 {
            return;
        }
        let ring = self.slots[usize::from(device.slot)].rings[dci as usize];
        if !ring.is_allocated() {
            return;
        }
        let _ = self.command(0, 0, (TRB_RESET_ENDPOINT << 10) | (dci << 16) | (u32::from(device.slot) << 24));
        let dequeue = ring.enqueue_address() | u64::from(ring.cycle);
        let _ = self.command(dequeue, 0, (TRB_SET_TR_DEQUEUE << 10) | (dci << 16) | (u32::from(device.slot) << 24));
    }

    pub fn get_descriptor(&mut self, device: &Device, kind: u8, index: u8, buffer: &mut [u8]) -> Result<usize, Error> {
        self.control_transfer(device, 0x80, 0x06, (u16::from(kind) << 8) | u16::from(index), 0, buffer)
    }

    pub fn set_configuration(&mut self, device: &Device, value: u8) -> Result<(), Error> {
        self.control_transfer(device, 0x00, 0x09, u16::from(value), 0, &mut []).map(|_| ())
    }

    /// Adds an interrupt-IN endpoint (a keyboard's report pipe) to the device
    /// (Configure Endpoint) and returns its device-context index.
    /// `interval` is the descriptor's bInterval.
    pub fn configure_interrupt_in(&mut self, device: &Device, endpoint: u8, max_packet: u16, interval: u8) -> Result<u32, Error> {
        let slot = usize::from(device.slot);
        let dci = u32::from(endpoint & 0x0f) * 2 + 1;
        if dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let ring = Ring::allocate().ok_or(Error::NoMemory)?;
        let (input, output) = (self.slots[slot].input, self.slots[slot].output);
        // xHCI wants the polling interval as an exponent of 125 us microframes: for
        // high/SuperSpeed it is bInterval-1; for low/full speed bInterval is in
        // milliseconds, so convert to microframes and take the log.
        let exponent = if device.speed >= 3 {
            u32::from(interval.clamp(1, 16)) - 1
        } else {
            (u32::from(interval.max(1)) * 8).ilog2().clamp(3, 10)
        };
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            (input as *mut u32).add(1).write_volatile(1 | (1 << dci)); // add: slot context + this endpoint
            // The slot context is the controller's current one, with "context entries"
            // raised to cover this endpoint.
            core::ptr::copy_nonoverlapping(output as *const u8, self.context_slot(input) as *mut u8, 32);
            let slot_context = self.context_slot(input) as *mut u32;
            let word0 = slot_context.read_volatile() & !(0x1f << 27);
            slot_context.write_volatile(word0 | (dci << 27));
            let context = self.context_endpoint(input, dci) as *mut u32;
            context.write_volatile(exponent << 16); // interval
            context.add(1).write_volatile((3 << 1) | (7 << 3) | (u32::from(max_packet) << 16)); // CErr 3, interrupt IN
            context.add(2).write_volatile(ring.base as u32 | 1);
            context.add(3).write_volatile((ring.base >> 32) as u32);
            context.add(4).write_volatile(u32::from(max_packet) | (u32::from(max_packet) << 16)); // average TRB length, max ESIT payload
        }
        self.slots[slot].rings[dci as usize] = ring;
        self.command(input, 0, (TRB_CONFIGURE_ENDPOINT << 10) | (u32::from(device.slot) << 24))?;
        Ok(dci)
    }

    /// Queues one buffer on an interrupt-IN endpoint and rings its doorbell: the
    /// controller completes it (an event) when the device next has data.
    pub fn queue_interrupt_in(&mut self, slot: u8, dci: u32, buffer: u64, length: u32) {
        let ring = &mut self.slots[usize::from(slot)].rings[dci as usize];
        ring.push(buffer, length, (TRB_NORMAL << 10) | TRB_IOC | TRB_ISP);
        self.ring_doorbell(slot, dci);
    }

    /// Adds an isochronous endpoint (Configure Endpoint) and returns its device-context index.
    /// `interval` is the descriptor's bInterval; for isochronous endpoints it is an exponent at every
    /// speed (the period is 2^(bInterval-1) frames at full speed, microframes at high speed). The
    /// slot's "context entries" only ever grows, so an audio interface can add its playback,
    /// feedback and capture endpoints one at a time.
    pub fn configure_isoc_endpoint(&mut self, device: &Device, endpoint: u8, max_packet: u16, interval: u8, ring: &Ring) -> Result<u32, Error> {
        let slot = usize::from(device.slot);
        let input_endpoint = endpoint & 0x80 != 0;
        let dci = u32::from(endpoint & 0x0f) * 2 + u32::from(input_endpoint);
        if dci < 2 || dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let (input, output) = (self.slots[slot].input, self.slots[slot].output);
        // Interval field: an exponent of 125 us microframes. High speed: bInterval-1. Full speed:
        // frames are 1 ms = 8 microframes, so (bInterval-1) + 3.
        let exponent = if device.speed >= 3 { u32::from(interval.clamp(1, 16)) - 1 } else { u32::from(interval.clamp(1, 16)) - 1 + 3 };
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            (input as *mut u32).add(1).write_volatile(1 | (1 << dci));
            core::ptr::copy_nonoverlapping(output as *const u8, self.context_slot(input) as *mut u8, 32);
            let slot_context = self.context_slot(input) as *mut u32;
            let word0 = slot_context.read_volatile();
            let entries = ((word0 >> 27) & 0x1f).max(dci);
            slot_context.write_volatile((word0 & !(0x1f << 27)) | (entries << 27));
            let context = self.context_endpoint(input, dci) as *mut u32;
            context.write_volatile(exponent << 16);
            // CErr 0 (isochronous transfers are never retried), endpoint type 5 (isoc IN) or 1 (isoc OUT).
            context.add(1).write_volatile(((if input_endpoint { 5 } else { 1 }) << 3) | (u32::from(max_packet) << 16));
            context.add(2).write_volatile(ring.base as u32 | 1);
            context.add(3).write_volatile((ring.base >> 32) as u32);
            context.add(4).write_volatile(u32::from(max_packet) | (u32::from(max_packet) << 16)); // average TRB length, max ESIT payload
        }
        self.command(input, 0, (TRB_CONFIGURE_ENDPOINT << 10) | (u32::from(device.slot) << 24))?;
        Ok(dci)
    }

    /// Stops an endpoint's transfers (Stop Endpoint command); queued ones are abandoned.
    pub fn stop_endpoint(&mut self, slot: u8, dci: u32) {
        let _ = self.command(0, 0, (TRB_STOP_ENDPOINT << 10) | (dci << 16) | (u32::from(slot) << 24));
    }

    /// Tells the controller to forget a stopped endpoint (the caller frees its transfer ring)
    /// (Configure Endpoint with its drop flag set).
    pub fn drop_endpoint(&mut self, device: &Device, dci: u32) {
        let slot = usize::from(device.slot);
        let input = self.slots[slot].input;
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            (input as *mut u32).write_volatile(1 << dci); // drop flags (D)
            (input as *mut u32).add(1).write_volatile(1); // A0: the slot context
            core::ptr::copy_nonoverlapping(self.slots[slot].output as *const u8, self.context_slot(input) as *mut u8, 32);
        }
        let _ = self.command(input, 0, (TRB_CONFIGURE_ENDPOINT << 10) | (u32::from(device.slot) << 24));
    }

    /// Adds a device's two bulk endpoints (Configure Endpoint). `*_endpoint` are endpoint
    /// addresses (bit 7 = IN); `burst` is the SuperSpeed companion's max burst. An endpoint
    /// number of 0 leaves that direction out (a device in a firmware-download state has only a bulk OUT).
    #[allow(clippy::too_many_arguments)]
    pub fn configure_bulk_endpoints(&mut self, device: &Device, in_endpoint: u8, in_max_packet: u16, in_burst: u8, out_endpoint: u8, out_max_packet: u16, out_burst: u8) -> Result<(), Error> {
        let slot = usize::from(device.slot);
        let in_dci = if in_endpoint & 0x0f != 0 { u32::from(in_endpoint & 0x0f) * 2 + 1 } else { 0 };
        let out_dci = u32::from(out_endpoint & 0x0f) * 2;
        if (in_dci == 0 && out_dci == 0) || in_dci >= DCI_COUNT as u32 || out_dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let in_ring = if in_dci != 0 { Ring::allocate().ok_or(Error::NoMemory)? } else { NO_RING };
        let out_ring = if out_dci != 0 { Ring::allocate().ok_or(Error::NoMemory)? } else { NO_RING };
        let (input, output) = (self.slots[slot].input, self.slots[slot].output);
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            let added = (u32::from(in_dci != 0) << in_dci) | (u32::from(out_dci != 0) << out_dci);
            (input as *mut u32).add(1).write_volatile(1 | added); // add the slot context and the endpoints
            core::ptr::copy_nonoverlapping(output as *const u8, self.context_slot(input) as *mut u8, 32);
            let slot_context = self.context_slot(input) as *mut u32;
            let word0 = slot_context.read_volatile();
            let entries = ((word0 >> 27) & 0x1f).max(in_dci.max(out_dci)); // keep what an earlier endpoint (audio) already needs
            slot_context.write_volatile((word0 & !(0x1f << 27)) | (entries << 27));
            for (dci, ring, endpoint_type, max_packet, burst) in [(in_dci, in_ring, 6u32, in_max_packet, in_burst), (out_dci, out_ring, 2u32, out_max_packet, out_burst)] {
                if dci == 0 {
                    continue;
                }
                let context = self.context_endpoint(input, dci) as *mut u32;
                context.add(1).write_volatile((3 << 1) | (endpoint_type << 3) | (u32::from(burst) << 8) | (u32::from(max_packet) << 16));
                context.add(2).write_volatile(ring.base as u32 | 1);
                context.add(3).write_volatile((ring.base >> 32) as u32);
                context.add(4).write_volatile(1024);
            }
        }
        if in_dci != 0 {
            self.slots[slot].rings[in_dci as usize] = in_ring;
        }
        if out_dci != 0 {
            self.slots[slot].rings[out_dci as usize] = out_ring;
        }
        self.command(input, 0, (TRB_CONFIGURE_ENDPOINT << 10) | (u32::from(device.slot) << 24))?;
        Ok(())
    }

    /// One bulk transfer between the device and the physical buffer at `buffer` (IN when
    /// bit 7 of `endpoint` is set). No TRB may cross a 64 KiB boundary, so a transfer
    /// is split into several chained TRBs. Returns the bytes moved.
    pub fn bulk_transfer(&mut self, device: &Device, endpoint: u8, buffer: u64, length: u32, max_packet: u16) -> Result<usize, Error> {
        const BOUNDARY: u64 = 64 * 1024;
        let data_in = endpoint & 0x80 != 0;
        let dci = u32::from(endpoint & 0x0f) * 2 + u32::from(data_in);
        if length == 0 || length > 1024 * 1024 || dci == 0 || dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let ring = &mut self.slots[usize::from(device.slot)].rings[dci as usize];
        if !ring.is_allocated() {
            return Err(Error::Unsupported);
        }
        let mut offset = 0u32;
        let mut last = 0;
        // (TRB address, offset into the buffer, length): turns a short packet's TRB address into a byte count.
        let mut pieces = [(0u64, 0u32, 0u32); 33];
        let mut piece_count = 0;
        while offset < length {
            let address = buffer + u64::from(offset);
            let piece = (length - offset).min((BOUNDARY - address % BOUNDARY) as u32);
            let after = length - offset - piece;
            // TD size: packets still to come after this TRB, capped at 31.
            let td_size = after.div_ceil(u32::from(max_packet.max(1))).min(31);
            let mut control = TRB_NORMAL << 10;
            control |= if after != 0 { 1 << 4 } else { TRB_IOC }; // chain, or interrupt on the last
            if data_in {
                control |= TRB_ISP;
            }
            last = ring.push(address, piece | (td_size << 17), control);
            pieces[piece_count] = (last, offset, piece);
            piece_count += 1;
            offset += piece;
        }
        self.ring_doorbell(device.slot, dci);
        let mut short = None;
        loop {
            let slot = device.slot;
            let event = self.wait_event(500, |event| event.kind() == TRB_TRANSFER_EVENT && event.slot() == slot && event.endpoint() == dci)?;
            match event.code() {
                CC_SUCCESS if event.parameter == last => break,
                CC_SHORT_PACKET => {
                    short = Some((event.parameter, event.status & 0x00ff_ffff));
                    break; // a short packet ends the whole TD: no further events
                }
                CC_SUCCESS => {}
                code => return Err(Error::Transfer(code)),
            }
        }
        Ok(match short {
            Some((trb, residual)) => pieces[..piece_count].iter().find(|piece| piece.0 == trb).map_or(length as usize, |&(_, at, piece)| (at + piece.saturating_sub(residual)) as usize),
            None => length as usize,
        })
    }

    /// Clears a halted bulk endpoint on both sides: the controller's (Reset Endpoint)
    /// and the device's (CLEAR_FEATURE ENDPOINT_HALT).
    pub fn clear_endpoint_halt(&mut self, device: &Device, endpoint: u8) {
        let dci = u32::from(endpoint & 0x0f) * 2 + u32::from(endpoint & 0x80 != 0);
        self.reset_endpoint(device, dci);
        let _ = self.control_transfer(device, 0x02, 0x01, 0, u16::from(endpoint), &mut []);
    }

    pub fn reset_stalled_endpoint(&mut self, slot: u8, dci: u32) {
        let device = Device { slot, port: 0, route: 0, tier: 0, tt: None, speed: 0, control_max_packet: 0 };
        self.reset_endpoint(&device, dci);
    }
}

impl Trb {
    /// For a transfer event: the bytes the transfer did NOT move (the short-packet residual).
    pub fn residual(&self) -> u32 {
        self.status & 0x00ff_ffff
    }
    /// For a transfer event: the address of the TRB it reports.
    pub fn trb_address(&self) -> u64 {
        self.parameter
    }
    pub fn is_transfer(&self) -> bool {
        self.kind() == TRB_TRANSFER_EVENT
    }
    pub fn slot_number(&self) -> u8 {
        self.slot()
    }
    pub fn endpoint_index(&self) -> u32 {
        self.endpoint()
    }
    pub fn completion(&self) -> u8 {
        self.code()
    }
}

pub const COMPLETION_SUCCESS: u8 = CC_SUCCESS;
pub const COMPLETION_SHORT: u8 = CC_SHORT_PACKET;
