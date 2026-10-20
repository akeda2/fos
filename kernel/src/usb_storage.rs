//! USB Mass Storage, Bulk-Only Transport (BOT): a USB stick as a block device, over
//! xHCI or EHCI. This is how a machine that boots from a stick reads its own boot
//! disk once the firmware's drivers are gone.
//!
//! BOT in one paragraph. A command is three bulk transfers: the host sends a 31-byte
//! *command block wrapper* (CBW: a tag, the data length and direction, and a SCSI
//! command descriptor block), then the data stage (if any) in the direction the CBW
//! announced, then reads a 13-byte *command status wrapper* (CSW) echoing the tag with
//! a status byte. The SCSI subset used: INQUIRY, TEST UNIT READY, REQUEST SENSE,
//! READ CAPACITY(10), READ(10), WRITE(10). Only 512-byte logical blocks and disks up to
//! 2 TiB are accepted ("reject rather than emulate"). A transport error runs BOT's
//! reset recovery (class reset, then clear both endpoints' halt).
//!
//! All transfers go through DMA frames below 4 GiB owned by this module, so the caller's
//! buffer can be anywhere and a 32-bit EHCI is as happy as an xHCI.

use crate::{memory, timer, xhci};
#[cfg(target_arch = "x86_64")]
use crate::ehci;

const CBW_LEN: usize = 31;
const CSW_LEN: usize = 13;
const MAX_DEVICES: usize = 4;
/// Largest data stage: 64 KiB = 128 sectors per SCSI command.
const DATA_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy)]
pub enum Error {
    Transport,
    Phase,
    CommandFailed,
    Unsupported,
}

/// What `find_endpoints` learns from a configuration descriptor.
#[derive(Clone, Copy)]
pub struct Endpoints {
    pub interface: u8,
    pub in_endpoint: u8,
    pub in_max_packet: u16,
    pub in_burst: u8,
    pub out_endpoint: u8,
    pub out_max_packet: u16,
    pub out_burst: u8,
}

/// Looks through a configuration descriptor for a Mass Storage / SCSI / Bulk-Only
/// interface (class 8, subclass 6, protocol 0x50) with a bulk IN and a bulk OUT endpoint.
pub fn find_endpoints(config: &[u8]) -> Option<Endpoints> {
    let mut at = 0;
    let (mut interface, mut inside) = (0u8, false);
    let mut in_endpoint: Option<(u8, u16, u8)> = None;
    let mut out_endpoint: Option<(u8, u16, u8)> = None;
    let mut last_was_in = false;
    while at + 1 < config.len() {
        let length = usize::from(config[at]);
        if length < 2 || at + length > config.len() {
            break;
        }
        match config[at + 1] {
            4 if length >= 9 => {
                if in_endpoint.is_some() && out_endpoint.is_some() {
                    break; // the first matching interface is complete
                }
                inside = config[at + 5] == 8 && config[at + 6] == 6 && config[at + 7] == 0x50;
                if inside {
                    interface = config[at + 2];
                    in_endpoint = None;
                    out_endpoint = None;
                }
            }
            5 if inside && length >= 7 && config[at + 3] & 3 == 2 => {
                let address = config[at + 2];
                let max_packet = u16::from_le_bytes([config[at + 4], config[at + 5]]) & 0x7ff;
                last_was_in = address & 0x80 != 0;
                if last_was_in {
                    in_endpoint = Some((address, max_packet, 0));
                } else {
                    out_endpoint = Some((address, max_packet, 0));
                }
            }
            0x30 if inside && length >= 3 => {
                // SuperSpeed endpoint companion: byte 2 is bMaxBurst for the endpoint just seen.
                let slot = if last_was_in { in_endpoint.as_mut() } else { out_endpoint.as_mut() };
                if let Some(endpoint) = slot {
                    endpoint.2 = config[at + 2];
                }
            }
            _ => {}
        }
        at += length;
    }
    let (in_endpoint, out_endpoint) = (in_endpoint?, out_endpoint?);
    Some(Endpoints { interface, in_endpoint: in_endpoint.0, in_max_packet: in_endpoint.1, in_burst: in_endpoint.2, out_endpoint: out_endpoint.0, out_max_packet: out_endpoint.1, out_burst: out_endpoint.2 })
}

#[derive(Clone, Copy)]
enum Host {
    Xhci { controller: usize, device: xhci::Device },
    #[cfg(target_arch = "x86_64")]
    Ehci { controller: usize, target: ehci::Target, root_port: u8, in_toggle: bool, out_toggle: bool },
}

#[derive(Clone, Copy)]
struct Msc {
    host: Host,
    endpoints: Endpoints,
    tag: u32,
    /// The INQUIRY data and the size, to recognise the same stick when it comes back after being unplugged.
    identity: [u8; 36],
    blocks: u64,
    /// The controller released this device's slot (its port was found changed and enumerated afresh). The handle names a slot
    /// number that may now be free (its rings are gone: a command through it would write to a null ring) or reused by the
    /// very device that came back (a command would succeed against the wrong connection, and the returning stick would look like
    /// a second disk). A stale handle is dead: it fails at once and is only ever replaced.
    stale: bool,
}

static mut DEVICES: [Option<Msc>; MAX_DEVICES] = [None; MAX_DEVICES];
/// DMA memory: one frame for CBW/CSW, 16 frames for the data stage.
static mut COMMAND_FRAME: u64 = 0;
static mut DATA_FRAME: u64 = 0;

fn dma() -> Option<(u64, u64)> {
    unsafe {
        if COMMAND_FRAME == 0 {
            COMMAND_FRAME = memory::allocate_contiguous(1, Some(1 << 32))?;
            DATA_FRAME = memory::allocate_contiguous((DATA_BYTES / 4096) as u64, Some(1 << 32))?;
        }
        Some((COMMAND_FRAME, DATA_FRAME))
    }
}

/// True when a USB disk is attached through xHCI controller `controller_index`.
pub fn uses_xhci(controller_index: usize) -> bool {
    (0..MAX_DEVICES).any(|slot| matches!(device(slot), Some(Msc { host: Host::Xhci { controller, .. }, .. }) if *controller == controller_index))
}

fn device(index: usize) -> Option<&'static mut Msc> {
    unsafe { (*core::ptr::addr_of_mut!(DEVICES)).get_mut(index)?.as_mut() }
}

impl Msc {
    /// The device has been unplugged (or unplugged and replugged, which gives it a new USB address, so this handle is as dead as
    /// if it were gone): its root port reports no device or a connection change since it was enumerated. Asking the port is
    /// instant; letting a transfer to a missing device time out takes seconds per attempt, and a filesystem lookup makes many.
    fn is_gone(&self) -> bool {
        if self.stale {
            return true;
        }
        match self.host {
            #[cfg(target_arch = "x86_64")]
            Host::Ehci { controller, root_port, .. } if root_port != 0 => {
                ehci::controller(controller).map_or(true, |c| !c.port_has_device(root_port) || c.port_connect_changed(root_port))
            }
            // Only "nothing is connected" is conclusive on xHCI; a change bit alone could come from a glitch on a healthy
            // SuperSpeed link, and failing a working boot disk at once would be worse than a slow failure. (A replug is still
            // handled: `usb rescan` sees the change bit and checks the old handle by really using it.)
            Host::Xhci { controller, device } if device.route == 0 => xhci::controller(controller).map_or(true, |c| c.port_status(device.port) & 1 == 0),
            #[cfg(target_arch = "x86_64")]
            Host::Ehci { controller, target, .. } if target.hub_address != 0 => {
                ehci::controller(controller).map_or(true, |c| c.hub_child_vanished(target.hub_address, target.hub_port))
            }
            Host::Xhci { controller, device } => crate::usb_hub::child_vanished(controller, device.port, device.route),
            #[allow(unreachable_patterns)]
            _ => false,
        }
    }

    /// One bulk transfer on the data (`data_in`) or command (OUT) pipe, on DMA memory.
    fn bulk(&mut self, data_in: bool, physical: u64, length: usize) -> Result<usize, Error> {
        let endpoints = self.endpoints;
        let (endpoint, max_packet) = if data_in { (endpoints.in_endpoint, endpoints.in_max_packet) } else { (endpoints.out_endpoint, endpoints.out_max_packet) };
        match &mut self.host {
            Host::Xhci { controller, device } => {
                let controller = xhci::controller(*controller).ok_or(Error::Transport)?;
                controller.bulk_transfer(device, endpoint, physical, length as u32, max_packet).map_err(|_| Error::Transport)
            }
            #[cfg(target_arch = "x86_64")]
            Host::Ehci { controller, target, in_toggle, out_toggle, .. } => {
                let controller = ehci::controller(*controller).ok_or(Error::Transport)?;
                let toggle = if data_in { in_toggle } else { out_toggle };
                controller.bulk_transfer(target, endpoint & 0x0f, max_packet, physical as u32, length, data_in, toggle).map_err(|_| Error::Transport)
            }
        }
    }

    /// BOT reset recovery: class reset, then clear halt on both bulk endpoints.
    fn recover(&mut self) {
        let endpoints = self.endpoints;
        match &mut self.host {
            Host::Xhci { controller, device } => {
                if let Some(controller) = xhci::controller(*controller) {
                    let _ = controller.control_transfer(device, 0x21, 0xff, 0, u16::from(endpoints.interface), &mut []);
                    controller.clear_endpoint_halt(device, endpoints.in_endpoint);
                    controller.clear_endpoint_halt(device, endpoints.out_endpoint);
                }
            }
            #[cfg(target_arch = "x86_64")]
            Host::Ehci { controller, target, in_toggle, out_toggle, .. } => {
                if let Some(controller) = ehci::controller(*controller) {
                    let _ = controller.control_transfer(target, 0x21, 0xff, 0, u16::from(endpoints.interface), &mut []);
                    for endpoint in [endpoints.in_endpoint, endpoints.out_endpoint] {
                        let _ = controller.control_transfer(target, 0x02, 0x01, 0, u16::from(endpoint), &mut []);
                    }
                }
                *in_toggle = false;
                *out_toggle = false;
            }
        }
    }

    /// One attempt at a full exchange: CBW out, optional data stage, CSW in. Data moves
    /// through the data DMA frame; returns the bytes the data stage moved.
    fn command_once(&mut self, cdb: &[u8], data: &mut [u8], data_in: bool) -> Result<usize, Error> {
        let (command, data_frame) = dma().ok_or(Error::Unsupported)?;
        if data.len() > DATA_BYTES {
            return Err(Error::Unsupported);
        }
        self.tag = self.tag.wrapping_add(1);
        let tag = self.tag;
        let mut cbw = [0u8; CBW_LEN];
        cbw[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes()); // "USBC"
        cbw[4..8].copy_from_slice(&tag.to_le_bytes());
        cbw[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes());
        cbw[12] = if data_in { 0x80 } else { 0 };
        cbw[14] = cdb.len() as u8;
        cbw[15..15 + cdb.len()].copy_from_slice(cdb);
        unsafe { core::ptr::copy_nonoverlapping(cbw.as_ptr(), command as *mut u8, CBW_LEN) };
        if self.bulk(false, command, CBW_LEN).is_err() {
            self.recover();
            return Err(Error::Transport);
        }
        let mut moved = 0;
        if !data.is_empty() {
            if !data_in {
                unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), data_frame as *mut u8, data.len()) };
            }
            match self.bulk(data_in, data_frame, data.len()) {
                Ok(length) => moved = length,
                Err(_) => {
                    self.recover();
                    return Err(Error::Transport);
                }
            }
            if data_in {
                unsafe { core::ptr::copy_nonoverlapping(data_frame as *const u8, data.as_mut_ptr(), moved.min(data.len())) };
            }
        }
        if self.bulk(true, command + 64, CSW_LEN).is_err() {
            self.recover();
            return Err(Error::Transport);
        }
        let csw = unsafe { core::ptr::read_volatile((command + 64) as *const [u8; CSW_LEN]) };
        if u32::from_le_bytes([csw[0], csw[1], csw[2], csw[3]]) != 0x5342_5355 || u32::from_le_bytes([csw[4], csw[5], csw[6], csw[7]]) != tag {
            self.recover();
            return Err(Error::Phase);
        }
        match csw[12] {
            0 => Ok(moved),
            1 => Err(Error::CommandFailed),
            _ => {
                self.recover();
                Err(Error::Phase)
            }
        }
    }

    /// Up to three attempts: a real stick occasionally times out one transfer inside an
    /// otherwise correct exchange. An ordinary SCSI failure is an answer, not a glitch.
    fn command(&mut self, cdb: &[u8], data: &mut [u8], data_in: bool) -> Result<usize, Error> {
        if self.is_gone() {
            return Err(Error::Transport);
        }
        let mut last = Error::Transport;
        for attempt in 0..3 {
            match self.command_once(cdb, data, data_in) {
                Ok(length) => return Ok(length),
                Err(Error::CommandFailed) => return Err(Error::CommandFailed),
                Err(error) => {
                    last = error;
                    if attempt < 2 {
                        timer::wait_ticks(1);
                    }
                }
            }
        }
        Err(last)
    }

    /// TEST UNIT READY until it succeeds (fresh media can report not-ready for a moment).
    fn wait_until_ready(&mut self) -> bool {
        for _ in 0..30 {
            if self.command(&[0u8; 6], &mut [], true).is_ok() {
                return true;
            }
            timer::wait_ticks(10);
        }
        false
    }

    fn inquiry(&mut self) -> [u8; 36] {
        let mut data = [0u8; 36];
        let _ = self.command(&[0x12, 0, 0, 0, 36, 0], &mut data, true);
        data
    }

    /// (block count, block size)
    fn read_capacity(&mut self) -> Result<(u64, u32), Error> {
        let mut data = [0u8; 8];
        self.command(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], &mut data, true)?;
        let last = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        Ok((u64::from(last) + 1, u32::from_be_bytes([data[4], data[5], data[6], data[7]])))
    }
}

fn cdb(opcode: u8, lba: u32, blocks: u16) -> [u8; 10] {
    let lba = lba.to_be_bytes();
    let blocks = blocks.to_be_bytes();
    [opcode, 0, lba[0], lba[1], lba[2], lba[3], 0, blocks[0], blocks[1], 0]
}

/// Reads whole sectors from USB disk `index`.
pub fn read(index: usize, lba: u64, buffer: &mut [u8]) -> Result<(), Error> {
    let msc = device(index).ok_or(Error::Unsupported)?;
    for (n, chunk) in buffer.chunks_mut(DATA_BYTES).enumerate() {
        let at = lba + (n * DATA_BYTES / 512) as u64;
        msc.command(&cdb(0x28, at as u32, (chunk.len() / 512) as u16), chunk, true)?;
    }
    Ok(())
}

/// Writes whole sectors to USB disk `index`.
pub fn write(index: usize, lba: u64, buffer: &[u8]) -> Result<(), Error> {
    let msc = device(index).ok_or(Error::Unsupported)?;
    let mut scratch = [0u8; 0];
    let _ = &mut scratch;
    for (n, chunk) in buffer.chunks(DATA_BYTES).enumerate() {
        let at = lba + (n * DATA_BYTES / 512) as u64;
        // The data stage copies out of the slice only; cast away constness for the shared signature.
        let chunk = unsafe { core::slice::from_raw_parts_mut(chunk.as_ptr() as *mut u8, chunk.len()) };
        msc.command(&cdb(0x2a, at as u32, (chunk.len() / 512) as u16), chunk, false)?;
    }
    Ok(())
}

/// Called when xHCI root port `port` of controller `controller_index` is about to have its slots released: every disk on that
/// port (a hub's devices share its root port) becomes stale (see `Msc::stale`). It stays registered, so that the same stick
/// coming back takes over its disk number.
pub fn mark_xhci_port_stale(controller_index: usize, port: u8) {
    for slot in 0..MAX_DEVICES {
        if let Some(Msc { host: Host::Xhci { controller, device }, stale, .. }) = device(slot) {
            if *controller == controller_index && device.port == port {
                *stale = true;
            }
        }
    }
}

/// The same for the disks behind one hub port: the device on `port` at hub-port `route` (and anything below it, when it is a
/// hub itself) went away or was replaced. Used by `usb_hub::scan`, which sees it in the hub's port status.
pub fn mark_xhci_route_stale(controller_index: usize, port: u8, route: u32) {
    let nibbles = (32 - route.leading_zeros()).div_ceil(4);
    let mask = if nibbles >= 8 { u32::MAX } else { (1u32 << (4 * nibbles)) - 1 };
    for slot in 0..MAX_DEVICES {
        if let Some(Msc { host: Host::Xhci { controller, device }, stale, .. }) = device(slot) {
            if *controller == controller_index && device.port == port && device.route & mask == route {
                *stale = true;
            }
        }
    }
}

/// The EHCI counterpart: the disks behind hub `hub_address` (only the one on `hub_port`, or all of them when it is 0) went
/// away or were replaced, so their handles are dead.
#[cfg(target_arch = "x86_64")]
pub fn mark_ehci_hub_stale(controller_index: usize, hub_address: u8, hub_port: u8) {
    for slot in 0..MAX_DEVICES {
        if let Some(Msc { host: Host::Ehci { controller, target, .. }, stale, .. }) = device(slot) {
            if *controller == controller_index && target.hub_address == hub_address && (hub_port == 0 || target.hub_port == hub_port) {
                *stale = true;
            }
        }
    }
}

/// Registers a freshly addressed xHCI device with a Mass Storage interface as a disk.
/// The caller has already issued SET_CONFIGURATION. Returns the block-layer disk number.
pub fn attach_xhci(controller_index: usize, controller: &mut xhci::Controller, device: xhci::Device, endpoints: Endpoints) -> Option<usize> {
    if let Err(error) = controller.configure_bulk_endpoints(&device, endpoints.in_endpoint, endpoints.in_max_packet, endpoints.in_burst, endpoints.out_endpoint, endpoints.out_max_packet, endpoints.out_burst) {
        crate::println!("USB storage: configuring the bulk endpoints failed ({error:?}).");
        return None;
    }
    register(Msc { host: Host::Xhci { controller: controller_index, device }, endpoints, tag: 0, identity: [0; 36], blocks: 0, stale: false }, "USB xHCI")
}

/// `root_port` is the EHCI root port the device sits on directly (1-based), or 0 behind a hub: only a direct device's removal
/// can be seen from the controller's port register, and that is what lets a vanished disk fail at once.
#[cfg(target_arch = "x86_64")]
pub fn attach_ehci(controller_index: usize, target: ehci::Target, root_port: u8, endpoints: Endpoints) -> Option<usize> {
    register(Msc { host: Host::Ehci { controller: controller_index, target, root_port, in_toggle: false, out_toggle: false }, endpoints, tag: 0, identity: [0; 36], blocks: 0, stale: false }, "USB EHCI")
}

fn register(mut msc: Msc, origin: &str) -> Option<usize> {
    dma()?;
    let inquiry = msc.inquiry();
    if inquiry[0] & 0x1f != 0 {
        return None; // not a direct-access block device (a CD drive, say)
    }
    if !msc.wait_until_ready() {
        crate::println!("USB storage: the device never became ready.");
        return None;
    }
    let Ok((blocks, block_size)) = msc.read_capacity() else {
        crate::println!("USB storage: READ CAPACITY failed.");
        return None;
    };
    if block_size != 512 {
        crate::println!("USB storage: {block_size}-byte blocks are not supported (512 only).");
        return None;
    }
    msc.identity = inquiry;
    msc.blocks = blocks;
    // The same stick coming back after an unplug or a bumped cable: the old handle points at a USB address that no longer
    // exists, so its commands fail. If a registered device has this identity (INQUIRY data and size) and no longer answers,
    // give it the new connection instead of registering a second disk: the disk keeps its number, and everything using it
    // (the boot volume, the slot metadata, files) works again. A healthy device with the same identity is a second, identical
    // stick, and gets a disk of its own.
    for slot in 0..MAX_DEVICES {
        let Some(old) = device(slot) else { continue };
        if old.identity == msc.identity && old.blocks == msc.blocks && old.read_capacity().is_err() {
            unsafe { DEVICES[slot] = Some(msc) };
            if let Some(disk) = crate::block::usb_disk(slot) {
                crate::println!("USB storage: {} is back; disk{} works again.", core::str::from_utf8(&inquiry[8..32]).unwrap_or("?").trim(), disk + 1);
                return Some(disk);
            }
        }
    }
    let slot = unsafe { (*core::ptr::addr_of_mut!(DEVICES)).iter().position(Option::is_none)? };
    unsafe { DEVICES[slot] = Some(msc) };
    let mut model = [b' '; 40];
    model[..24].copy_from_slice(&inquiry[8..32]); // vendor (8) + product (16)
    crate::block::register(crate::block::Backend::Usb(slot), blocks, model, origin)
}
