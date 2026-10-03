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

use crate::{apic, memory, paging, pci};

pub const MAX_CONTROLLERS: usize = 4;
pub const MAX_SLOTS: usize = 8;
const RING_TRBS: usize = 256;
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
const TRB_CYCLE: u32 = 1;
const TRB_TOGGLE_CYCLE: u32 = 2;
const TRB_IOC: u32 = 1 << 5;
const TRB_IMMEDIATE_DATA: u32 = 1 << 6;
const TRB_ISP: u32 = 1 << 2;
const TRB_DIR_IN: u32 = 1 << 16;
const CC_SUCCESS: u8 = 1;
const CC_SHORT_PACKET: u8 = 13;

#[derive(Debug, Clone, Copy)]
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

/// A producer ring: one frame of TRBs whose last entry links back to the start.
#[derive(Clone, Copy)]
struct Ring {
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
    fn allocate() -> Option<Ring> {
        let base = frame()?;
        write_trb(base + (RING_TRBS as u64 - 1) * TRB_BYTES, &Trb { parameter: base, status: 0, control: TRB_LINK << 10 | TRB_TOGGLE_CYCLE });
        Some(Ring { base, enqueue: 0, cycle: true })
    }

    fn is_allocated(&self) -> bool {
        self.base != 0
    }

    fn enqueue_address(&self) -> u64 {
        self.base + self.enqueue as u64 * TRB_BYTES
    }

    /// Queues one TRB (`control` without its cycle bit); returns its address, which is
    /// how its completion event names it.
    fn push(&mut self, parameter: u64, status: u32, control: u32) -> u64 {
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
struct Slot {
    used: bool,
    port: u8,
    speed: u8,
    output: u64,
    input: u64,
    rings: [Ring; DCI_COUNT],
}

const EMPTY_SLOT: Slot = Slot { used: false, port: 0, speed: 0, output: 0, input: 0, rings: [NO_RING; DCI_COUNT] };

/// A device with a slot and an address.
#[derive(Clone, Copy)]
pub struct Device {
    pub slot: u8,
    pub port: u8,
    /// PORTSC speed ID: 1 full, 2 low, 3 high, 4 SuperSpeed, 5 SuperSpeed+.
    pub speed: u8,
    pub control_max_packet: u16,
}

pub struct Controller {
    pub pci: pci::Device,
    mmio: usize,
    operational: usize,
    runtime: usize,
    doorbells: usize,
    pub ports: u8,
    /// Context records are 64 bytes instead of 32 when HCCPARAMS1.CSZ is set.
    context_bytes: u64,
    event_ring: u64,
    dcbaa: u64,
    command_ring: Ring,
    event_index: usize,
    event_cycle: bool,
    slots: [Slot; MAX_SLOTS + 1],
}

static mut CONTROLLERS: [Option<Controller>; MAX_CONTROLLERS] = [const { None }; MAX_CONTROLLERS];
static mut COUNT: usize = 0;

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
        match Controller::init(device) {
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
    fn init(device: pci::Device) -> Result<Controller, Error> {
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
        let event_ring = frame().ok_or(Error::NoMemory)?;
        let segment_table = frame().ok_or(Error::NoMemory)?;
        unsafe {
            let entry = segment_table as *mut u32;
            entry.write_volatile(event_ring as u32);
            entry.add(1).write_volatile((event_ring >> 32) as u32);
            entry.add(2).write_volatile(RING_TRBS as u32);
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
            event_ring,
            dcbaa,
            command_ring,
            event_index: 0,
            event_cycle: true,
            slots: [EMPTY_SLOT; MAX_SLOTS + 1],
        };
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
        apic::wait_ticks(15);
        Ok(controller)
    }

    fn port_register(&self, port: u8) -> usize {
        self.operational + OP_PORTS + (usize::from(port) - 1) * 0x10
    }

    pub fn port_status(&self, port: u8) -> u32 {
        read32(self.port_register(port))
    }

    pub fn slot_in_use_for_port(&self, port: u8) -> bool {
        self.slots[1..].iter().any(|slot| slot.used && slot.port == port)
    }

    fn ring_doorbell(&self, slot: u8, target: u32) {
        write32(self.doorbells + usize::from(slot) * 4, target);
    }

    /// The next event the controller has written (its cycle bit matches ours), if any.
    pub fn next_event(&mut self) -> Option<Trb> {
        let at = self.event_ring + self.event_index as u64 * TRB_BYTES;
        let trb = read_trb(at);
        if (trb.control & TRB_CYCLE != 0) != self.event_cycle {
            return None;
        }
        self.event_index += 1;
        if self.event_index == RING_TRBS {
            self.event_index = 0;
            self.event_cycle = !self.event_cycle;
        }
        let dequeue = self.event_ring + self.event_index as u64 * TRB_BYTES;
        write64(self.runtime + 0x20 + 0x18, dequeue | (1 << 3)); // bit 3: clear "event handler busy"
        Some(trb)
    }

    /// Waits for an event accepted by `wanted`. Anything else (port changes, reports
    /// from keyboards already running) goes to the background handler.
    fn wait_event(&mut self, ticks: u64, wanted: impl Fn(&Trb) -> bool) -> Result<Trb, Error> {
        let start = apic::ticks();
        loop {
            crate::debug::pet();
            while let Some(event) = self.next_event() {
                if wanted(&event) {
                    return Ok(event);
                }
                crate::usb_hid::background_event(self, &event);
            }
            if apic::ticks().wrapping_sub(start) >= ticks {
                return Err(Error::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    fn command(&mut self, parameter: u64, status: u32, control: u32) -> Result<Trb, Error> {
        let at = self.command_ring.push(parameter, status, control);
        self.ring_doorbell(0, 0);
        let event = self.wait_event(TIMEOUT_TICKS, |event| event.kind() == TRB_COMMAND_COMPLETION && event.parameter == at)?;
        match event.code() {
            CC_SUCCESS => Ok(event),
            code => Err(Error::Command(code)),
        }
    }

    fn context_slot(&self, context: u64) -> u64 {
        context + self.context_bytes
    }

    fn context_endpoint(&self, context: u64, dci: u32) -> u64 {
        context + self.context_bytes * (1 + u64::from(dci))
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
            apic::wait_ticks(3); // 10 ms reset recovery
        }
        let status = read32(register);
        if status & PORTSC_ENABLED == 0 || status & PORTSC_CONNECTED == 0 {
            return Err(Error::NoDevice);
        }
        let speed = ((status >> 10) & 0xf) as u8;
        let max_packet: u16 = match speed {
            2 => 8,
            1 | 3 => 64,
            _ => 512,
        };
        let event = self.command(0, 0, TRB_ENABLE_SLOT << 10)?;
        let slot = event.slot();
        if slot == 0 || usize::from(slot) > MAX_SLOTS {
            return Err(Error::Unsupported);
        }
        let result = self.address_device(slot, port, speed, max_packet);
        if result.is_err() {
            self.release_slot(slot);
        }
        result
    }

    fn address_device(&mut self, slot: u8, port: u8, speed: u8, max_packet: u16) -> Result<Device, Error> {
        let input = frame().ok_or(Error::NoMemory)?;
        let output = frame().ok_or(Error::NoMemory)?;
        let control_ring = Ring::allocate().ok_or(Error::NoMemory)?;
        let entry = &mut self.slots[usize::from(slot)];
        *entry = EMPTY_SLOT;
        entry.used = true;
        entry.port = port;
        entry.speed = speed;
        entry.input = input;
        entry.output = output;
        entry.rings[1] = control_ring;
        unsafe { (self.dcbaa as *mut u64).add(usize::from(slot)).write_volatile(output) };
        unsafe {
            // Input context: add the slot context (A0) and the default pipe (A1).
            (input as *mut u32).add(1).write_volatile(0b11);
            let slot_context = self.context_slot(input) as *mut u32;
            slot_context.write_volatile((1 << 27) | (u32::from(speed) << 20)); // one context entry, speed
            slot_context.add(1).write_volatile(u32::from(port) << 16); // root hub port number
            let endpoint = self.context_endpoint(input, 1) as *mut u32;
            endpoint.add(1).write_volatile((3 << 1) | (4 << 3) | (u32::from(max_packet) << 16)); // CErr 3, control, max packet
            endpoint.add(2).write_volatile(control_ring.base as u32 | 1); // dequeue pointer + cycle state
            endpoint.add(3).write_volatile((control_ring.base >> 32) as u32);
            endpoint.add(4).write_volatile(8);
        }
        self.command(input, 0, (TRB_ADDRESS_DEVICE << 10) | (u32::from(slot) << 24))?;
        Ok(Device { slot, port, speed, control_max_packet: max_packet })
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
        if data.len() > 1024 {
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

    /// Adds a device's two bulk endpoints (Configure Endpoint). `*_endpoint` are endpoint
    /// addresses (bit 7 = IN); `burst` is the SuperSpeed companion's max burst.
    #[allow(clippy::too_many_arguments)]
    pub fn configure_bulk_endpoints(&mut self, device: &Device, in_endpoint: u8, in_max_packet: u16, in_burst: u8, out_endpoint: u8, out_max_packet: u16, out_burst: u8) -> Result<(), Error> {
        let slot = usize::from(device.slot);
        let in_dci = u32::from(in_endpoint & 0x0f) * 2 + 1;
        let out_dci = u32::from(out_endpoint & 0x0f) * 2;
        if in_dci >= DCI_COUNT as u32 || out_dci == 0 || out_dci >= DCI_COUNT as u32 {
            return Err(Error::Unsupported);
        }
        let in_ring = Ring::allocate().ok_or(Error::NoMemory)?;
        let out_ring = Ring::allocate().ok_or(Error::NoMemory)?;
        let (input, output) = (self.slots[slot].input, self.slots[slot].output);
        unsafe {
            core::ptr::write_bytes(input as *mut u8, 0, memory::FRAME as usize);
            (input as *mut u32).add(1).write_volatile(1 | (1 << in_dci) | (1 << out_dci)); // add the slot context and both endpoints
            core::ptr::copy_nonoverlapping(output as *const u8, self.context_slot(input) as *mut u8, 32);
            let slot_context = self.context_slot(input) as *mut u32;
            let word0 = slot_context.read_volatile() & !(0x1f << 27);
            slot_context.write_volatile(word0 | (in_dci.max(out_dci) << 27));
            for (dci, ring, endpoint_type, max_packet, burst) in [(in_dci, in_ring, 6u32, in_max_packet, in_burst), (out_dci, out_ring, 2u32, out_max_packet, out_burst)] {
                let context = self.context_endpoint(input, dci) as *mut u32;
                context.add(1).write_volatile((3 << 1) | (endpoint_type << 3) | (u32::from(burst) << 8) | (u32::from(max_packet) << 16));
                context.add(2).write_volatile(ring.base as u32 | 1);
                context.add(3).write_volatile((ring.base >> 32) as u32);
                context.add(4).write_volatile(1024);
            }
        }
        self.slots[slot].rings[in_dci as usize] = in_ring;
        self.slots[slot].rings[out_dci as usize] = out_ring;
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
        let device = Device { slot, port: 0, speed: 0, control_max_packet: 0 };
        self.reset_endpoint(&device, dci);
    }
}

impl Trb {
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
