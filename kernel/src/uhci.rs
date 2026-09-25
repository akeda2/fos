//! Minimal UHCI (USB 1.1) host controller driver: brings up a controller
//! and enumerates whatever's plugged into its root ports, reading back
//! real device/configuration descriptors from hardware. See AGENTS.md's
//! "USB" section for the design rationale (why UHCI only, not EHCI/xHCI).
//!
//! Register offsets, `USBCMD`/`USBSTS`/`PORTSC` bits, and the Transfer
//! Descriptor/Queue Head layout below were cross-checked directly against
//! QEMU's own UHCI emulation source (`hw/usb/hcd-uhci.c` and
//! `include/hw/usb/uhci-regs.h`) -- the same "verify against a real
//! reference rather than memory alone" discipline that caught the NE2000
//! header-byte-order bug elsewhere in this project.
//!
//! Two transfer types are implemented: control transfers (enumeration:
//! `GET_DESCRIPTOR`/`SET_ADDRESS`/`SET_CONFIGURATION`/`SET_INTERFACE`) and
//! isochronous OUT transfers (`IsocOutStream`, for USB Audio Class
//! playback -- see the `usb play` shell command). Isochronous IN
//! (capture) and any `audio.rs`/tape-ABI integration are explicit
//! follow-on work -- see AGENTS.md.

use crate::arch::x86::{inw, outw};
use crate::interrupts;
use crate::pci::{self, PciAddress};
use crate::pmem;

// ---- I/O register offsets, from the controller's I/O BAR base ----
const REG_USBCMD: u16 = 0x00;
const REG_USBSTS: u16 = 0x02;
const REG_USBINTR: u16 = 0x04;
const REG_FRNUM: u16 = 0x06;
const REG_FRBASEADD: u16 = 0x08;
const REG_PORTSC1: u16 = 0x10;
const REG_PORTSC2: u16 = 0x12;

const CMD_RS: u16 = 1 << 0;
const CMD_HCRESET: u16 = 1 << 1;
const CMD_GRESET: u16 = 1 << 2;

const STS_HCHALTED: u16 = 1 << 5;

const PORT_CCS: u16 = 1 << 0;
const PORT_EN: u16 = 1 << 2;
const PORT_LSDA: u16 = 1 << 8;
const PORT_RESET: u16 = 1 << 9;

/// QEMU's `piix3-usb-uhci` (and every real UHCI controller this project's
/// era of hardware uses) implements exactly two root ports.
pub const UHCI_PORTS: u8 = 2;

// ---- Frame List / Queue Head / Transfer Descriptor layout ----
const FRAME_LIST_ENTRIES: usize = 1024;
/// `USBFRNUM` is an 11-bit free-running frame counter, but only its low
/// 10 bits select a Frame List entry (`FRAME_LIST_ENTRIES` is 1024, not
/// 2048) -- confirmed directly against QEMU's own
/// `frame_addr = fl_base + ((frnum & 0x3ff) << 2)` (`hw/usb/hcd-uhci.c`),
/// not assumed from the register's full bit width.
const FRAME_INDEX_MASK: u16 = 0x3ff;
const LINK_TERMINATE: u32 = 1 << 0;
const LINK_QH_SELECT: u32 = 1 << 1;
const LINK_DEPTH_FIRST: u32 = 1 << 2;

const TD_CTRL_ERROR_COUNTER_SHIFT: u32 = 27;
/// "Isochronous Select": tells hardware this TD gets exactly one try,
/// with no retry/error-counter semantics, and unconditionally clears
/// `Active` on completion regardless of success -- confirmed against
/// QEMU's `uhci_complete_td` (`if (td->ctrl & TD_CTRL_IOS) { td->ctrl &=
/// ~TD_CTRL_ACTIVE; }`, checked *before* any success/failure branch).
const TD_CTRL_IOS: u32 = 1 << 25;
const TD_CTRL_IOC: u32 = 1 << 24;
const TD_CTRL_ACTIVE: u32 = 1 << 23;
const TD_CTRL_STALL: u32 = 1 << 22;
const TD_CTRL_BUFFER_ERROR: u32 = 1 << 21;
const TD_CTRL_BABBLE: u32 = 1 << 20;
const TD_CTRL_TIMEOUT_CRC: u32 = 1 << 18;
const TD_CTRL_ERROR_MASK: u32 =
    TD_CTRL_STALL | TD_CTRL_BUFFER_ERROR | TD_CTRL_BABBLE | TD_CTRL_TIMEOUT_CRC;
/// Give every transaction UHCI's usual 3-retry budget before the hardware
/// gives up and marks the TD with an error (NAKs do not consume this --
/// the hardware keeps retrying a NAK'd transaction on its own). Not used
/// by isochronous TDs, which the `TD_CTRL_IOS` bit above already exempts
/// from any retry behavior.
const TD_CTRL_ERROR_COUNTER_MAX: u32 = 3 << TD_CTRL_ERROR_COUNTER_SHIFT;
/// The 11-bit length field's own encoding (shared by `ActLen` here and
/// `MaxLen` in the token word below): length-minus-one, with the all-ones
/// value reserved to mean a zero-length packet, since -1 has nowhere else
/// to go in an unsigned field.
const TD_LEN_MASK: u32 = 0x7ff;
const TD_LEN_ZERO: u32 = 0x7ff;

const TD_TOKEN_MAXLEN_SHIFT: u32 = 21;
const TD_TOKEN_DATA_TOGGLE: u32 = 1 << 19;
const TD_TOKEN_DEVICE_SHIFT: u32 = 8;
/// Bits 18:15 of the token word -- confirmed against QEMU's own
/// extraction (`uint8_t ep_id = (td->token >> 15) & 0xf;`,
/// `hw/usb/hcd-uhci.c`), not assumed. Every control transfer so far
/// targets endpoint 0 (left at its default 0 by simply never OR-ing this
/// in); isochronous streaming needs a real, usually nonzero endpoint
/// number.
const TD_TOKEN_ENDPOINT_SHIFT: u32 = 15;

const PID_SETUP: u32 = 0x2d;
const PID_IN: u32 = 0x69;
const PID_OUT: u32 = 0xe1;

const USB_REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const USB_REQUEST_SET_ADDRESS: u8 = 0x05;
const USB_REQUEST_SET_INTERFACE: u8 = 0x0b;
const USB_REQUEST_SET_CONFIGURATION: u8 = 0x09;
pub const USB_DESCRIPTOR_DEVICE: u8 = 0x01;
pub const USB_DESCRIPTOR_CONFIGURATION: u8 = 0x02;

/// How many Frame List entries an `IsocOutStream` keeps primed ahead of
/// hardware's own position (`USBFRNUM`) at all times -- 16 ms of
/// look-ahead, comfortably tolerant of ordinary call overhead in the
/// foreground polling loop that drives `IsocOutStream::service`.
const ISOC_TD_COUNT: usize = 16;
/// A small safety margin (frames) between the controller's current
/// position and the very first Frame List entry `start_isoc_out` primes,
/// so the first fill pass isn't racing a frame about to be serviced.
const STARTUP_GUARD_FRAMES: u16 = 4;

/// A Transfer Descriptor's exact 16-byte on-the-wire (well, on-the-bus)
/// layout: `link` (next TD/QH or terminate), `control` (status bits, error
/// counter, actual length), `token` (PID, device address, endpoint, data
/// toggle, max length), `buffer` (physical data pointer).
#[repr(C)]
struct RawTd {
    link: u32,
    control: u32,
    token: u32,
    buffer: u32,
}

/// A Queue Head's exact 8-byte layout: `link` (next QH, or terminate) and
/// `element` (first TD in this queue, or terminate).
#[repr(C)]
struct RawQh {
    link: u32,
    element: u32,
}

/// UHCI's 11-bit length fields encode length-minus-one; `0x7ff` specially
/// means a zero-length packet.
fn encode_len(len: usize) -> u32 {
    if len == 0 {
        TD_LEN_ZERO
    } else {
        (len as u32 - 1) & TD_LEN_MASK
    }
}

fn decode_actlen(control: u32) -> usize {
    let raw = control & TD_LEN_MASK;
    if raw == TD_LEN_ZERO {
        0
    } else {
        (raw + 1) as usize
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum Error {
    NotPresent,
    InitFailed,
    /// A control transfer's TD chain halted on a real error (not a NAK,
    /// which the hardware already retries on its own) -- STALL, buffer
    /// error, babble, or a CRC/timeout that exhausted its retry budget.
    TransferFailed,
    /// The transfer did not complete within this driver's bounded poll --
    /// either the device never responded, or (more likely while this is
    /// new code) the TD chain was built incorrectly.
    Timeout,
}

pub const MAX_INSTANCES: usize = 4;
/// One page's worth of 16-byte Transfer Descriptor slots is far more than
/// one control transfer's SETUP+DATA+STATUS chain ever needs even for a
/// large configuration descriptor read in 8-byte packets; this just bounds
/// the fixed array `control_transfer` builds each chain into.
const TD_POOL_CAPACITY: usize = 34;
/// Largest descriptor this driver reads in one shot -- comfortably above
/// any configuration descriptor a simple, few-interface USB Audio Class
/// device reports (this is an enumeration/reporting tool, not a general
/// USB stack; a device with an exotic, very large configuration
/// descriptor would simply have its dump truncated).
pub const MAX_DESCRIPTOR_BYTES: usize = 255;

pub struct Uhci {
    instance: usize,
    io_base: u16,
    irq: Option<u8>,
    frame_list_addr: u32,
    /// One page holding this controller's single control Queue Head
    /// (at a fixed offset) and its Transfer Descriptor pool -- one
    /// control pipeline is enough for enumerating whatever is plugged
    /// into either root port, one device at a time.
    qh_td_pool_addr: u32,
    /// A second, dedicated page of `ISOC_TD_COUNT` Transfer Descriptors
    /// for isochronous streaming -- kept entirely separate from the
    /// control pool above, since isochronous TDs are never linked from
    /// a Queue Head and are indexed completely differently (a small
    /// rotating pool driven by `IsocOutStream`, not a bounded per-call
    /// chain).
    isoc_td_pool_addr: u32,
}

impl Uhci {
    pub fn instance(&self) -> usize {
        self.instance
    }

    pub fn io_base(&self) -> u16 {
        self.io_base
    }

    pub fn irq(&self) -> Option<u8> {
        self.irq
    }

    fn td_addr(&self, index: usize) -> u32 {
        self.qh_td_pool_addr + (index as u32) * 16
    }

    fn qh_addr(&self) -> u32 {
        self.td_addr(TD_POOL_CAPACITY)
    }

    fn isoc_td_addr(&self, slot: usize) -> u32 {
        self.isoc_td_pool_addr + (slot as u32) * 16
    }

    fn frame_list_entry_addr(&self, index: u16) -> u32 {
        self.frame_list_addr + u32::from(index) * 4
    }

    fn write_frame_list_entry(&self, index: u16, value: u32) {
        unsafe { core::ptr::write_volatile(self.frame_list_entry_addr(index) as *mut u32, value) }
    }

    /// The controller's free-running 11-bit frame counter, incremented
    /// once per real millisecond once `CMD_RS` is set. Callers index the
    /// Frame List with `& FRAME_INDEX_MASK` (its low 10 bits), not the
    /// raw value -- see `FRAME_INDEX_MASK`'s own doc comment.
    fn read_frame_number(&self) -> u16 {
        self.read_reg16(REG_FRNUM) & 0x7ff
    }

    fn read_reg16(&self, offset: u16) -> u16 {
        unsafe { inw(self.io_base + offset) }
    }

    fn write_reg16(&self, offset: u16, value: u16) {
        unsafe { outw(self.io_base + offset, value) }
    }

    fn write_reg32(&self, offset: u16, value: u32) {
        unsafe {
            outw(self.io_base + offset, (value & 0xffff) as u16);
            outw(self.io_base + offset + 2, (value >> 16) as u16);
        }
    }

    fn read_td_control(&self, index: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.td_addr(index) + 4) as *const u32) }
    }

    fn write_td(&self, index: usize, link: u32, control: u32, token: u32, buffer: u32) {
        let ptr = self.td_addr(index) as *mut RawTd;
        unsafe {
            core::ptr::write_volatile(&raw mut (*ptr).link, link);
            core::ptr::write_volatile(&raw mut (*ptr).token, token);
            core::ptr::write_volatile(&raw mut (*ptr).buffer, buffer);
            // Control (including the Active bit) written last: this is
            // the field hardware polls, so every other field must already
            // be in place before the transfer can be allowed to start.
            core::ptr::write_volatile(&raw mut (*ptr).control, control);
        }
    }

    /// Writes one isochronous TD into the dedicated isoc pool. Unlike
    /// `write_td` (the control pool), the `link` field is always
    /// `LINK_TERMINATE`: an isochronous TD is never chained to another
    /// TD or a Queue Head -- each Frame List entry a stream owns points
    /// straight at one of these.
    fn write_isoc_td(&self, slot: usize, control: u32, token: u32, buffer: u32) {
        let ptr = self.isoc_td_addr(slot) as *mut RawTd;
        unsafe {
            core::ptr::write_volatile(&raw mut (*ptr).link, LINK_TERMINATE);
            core::ptr::write_volatile(&raw mut (*ptr).token, token);
            core::ptr::write_volatile(&raw mut (*ptr).buffer, buffer);
            core::ptr::write_volatile(&raw mut (*ptr).control, control);
        }
    }

    fn read_isoc_td_control(&self, slot: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.isoc_td_addr(slot) + 4) as *const u32) }
    }

    fn write_qh(&self, link: u32, element: u32) {
        let ptr = self.qh_addr() as *mut RawQh;
        unsafe {
            core::ptr::write_volatile(&raw mut (*ptr).link, link);
            core::ptr::write_volatile(&raw mut (*ptr).element, element);
        }
    }

    /// Global Reset, then Host Controller Reset, then program the Frame
    /// List and start the controller with every frame pointing at the one
    /// control Queue Head (which starts out empty -- Terminate set).
    fn reset_and_start(&self) {
        self.write_reg16(REG_USBCMD, CMD_GRESET);
        wait_ticks(2);
        self.write_reg16(REG_USBCMD, 0);

        self.write_reg16(REG_USBCMD, CMD_HCRESET);
        for _ in 0..1000 {
            if self.read_reg16(REG_USBCMD) & CMD_HCRESET == 0 {
                break;
            }
        }

        self.write_reg16(REG_USBINTR, 0);
        self.write_qh(LINK_TERMINATE, LINK_TERMINATE);
        let frame_list = self.frame_list_addr as *mut u32;
        let qh_link = self.qh_addr() | LINK_QH_SELECT;
        for i in 0..FRAME_LIST_ENTRIES {
            unsafe { core::ptr::write_volatile(frame_list.add(i), qh_link) };
        }
        self.write_reg32(REG_FRBASEADD, self.frame_list_addr);
        self.write_reg16(REG_USBCMD, CMD_RS);
    }

    fn port_reg(&self, port: u8) -> u16 {
        if port == 0 { REG_PORTSC1 } else { REG_PORTSC2 }
    }

    /// Whether the controller is actively running its schedule (the
    /// complement of `USBSTS.HCHALTED`) -- printed by the `usb` shell
    /// command as a basic "is this controller alive at all" sanity check.
    pub fn is_running(&self) -> bool {
        self.read_reg16(REG_USBSTS) & STS_HCHALTED == 0
    }

    /// Drives the USB port reset sequence: assert Reset for >=10 ms (two
    /// PIT ticks at 100 Hz comfortably covers that), release it, then wait
    /// for the port to report Enabled. Returns whether the device that was
    /// present is low-speed (`LSDA`).
    pub fn reset_port(&self, port: u8) -> Result<bool, Error> {
        let reg = self.port_reg(port);
        self.write_reg16(reg, PORT_RESET);
        wait_ticks(2);
        self.write_reg16(reg, 0);
        wait_ticks(1);

        let status = self.read_reg16(reg);
        if status & PORT_CCS == 0 {
            return Err(Error::NotPresent);
        }

        // Unlike some other host-controller families, UHCI never
        // auto-enables a port once reset completes -- the spec says
        // "software sets this bit to 1 to enable the port", so this is
        // an explicit request, not something to just poll for. Clear the
        // (write-to-clear) change bits this sequence itself latched at
        // the same time, so a later poll for a *new* connect/enable
        // event isn't confused by this one.
        self.write_reg16(reg, status | PORT_EN);

        for _ in 0..100 {
            let status = self.read_reg16(reg);
            if status & PORT_CCS == 0 {
                return Err(Error::NotPresent);
            }
            if status & PORT_EN != 0 {
                // Write back the exact value just read: CSC/ENC are
                // write-1-to-clear, so this clears whichever of them is
                // currently set, while every other bit (crucially EN
                // itself) is rewritten with its own current value rather
                // than dropped -- PORTSC's R/W bits are ordinary
                // read/write storage, not sticky-on-zero, so a write that
                // *omits* EN actually disables the port again.
                self.write_reg16(reg, status);
                // USB 2.0 7.1.7.3's reset recovery time (TRSTRCY, >=10 ms)
                // before the host may address the device.
                wait_ticks(1);
                return Ok(status & PORT_LSDA != 0);
            }
            wait_ticks(1);
        }
        Err(Error::Timeout)
    }

    /// Builds and runs one control transfer's TD chain (SETUP, then zero
    /// or more data-stage TDs, then a zero-length STATUS TD), polling for
    /// completion. `data` is the caller's buffer for an IN transfer, or
    /// the bytes to send for an OUT transfer (`data_is_in` selects which);
    /// pass an empty slice for a no-data control transfer (e.g.
    /// `SET_ADDRESS`). Returns the number of data bytes actually
    /// transferred.
    fn control_transfer(
        &self,
        device_addr: u8,
        max_packet: u8,
        setup: &[u8; 8],
        data: &mut [u8],
        data_is_in: bool,
    ) -> Result<usize, Error> {
        let max_packet = usize::from(max_packet).max(1);

        // Fills TD pool slots 0..slot with SETUP, then each data-stage
        // packet, then STATUS; the link-patching loop below then chains
        // slot i to slot i+1 once the final chain length is known.
        let mut slot = 0usize;
        let setup_addr = setup.as_ptr() as u32;
        self.write_td(
            slot,
            0, // patched below once the chain length is known
            TD_CTRL_ACTIVE | TD_CTRL_ERROR_COUNTER_MAX,
            PID_SETUP
                | (u32::from(device_addr) << TD_TOKEN_DEVICE_SHIFT)
                | (encode_len(8) << TD_TOKEN_MAXLEN_SHIFT),
            setup_addr,
        );
        slot += 1;

        let data_pid = if data_is_in { PID_IN } else { PID_OUT };
        let mut toggle = TD_TOKEN_DATA_TOGGLE; // first data packet is always toggle 1
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = (data.len() - offset).min(max_packet);
            let buffer_addr = unsafe { data.as_mut_ptr().add(offset) } as u32;
            self.write_td(
                slot,
                0,
                TD_CTRL_ACTIVE | TD_CTRL_ERROR_COUNTER_MAX,
                data_pid
                    | (u32::from(device_addr) << TD_TOKEN_DEVICE_SHIFT)
                    | toggle
                    | (encode_len(chunk) << TD_TOKEN_MAXLEN_SHIFT),
                buffer_addr,
            );
            toggle ^= TD_TOKEN_DATA_TOGGLE;
            offset += chunk;
            slot += 1;
            if slot >= TD_POOL_CAPACITY - 1 {
                return Err(Error::InitFailed);
            }
        }

        // STATUS stage: opposite direction from the data stage (or an IN,
        // for a no-data request like SET_ADDRESS), always zero-length,
        // always data toggle 1.
        let status_pid = if data.is_empty() {
            PID_IN
        } else if data_is_in {
            PID_OUT
        } else {
            PID_IN
        };
        self.write_td(
            slot,
            LINK_TERMINATE,
            TD_CTRL_ACTIVE | TD_CTRL_ERROR_COUNTER_MAX | TD_CTRL_IOC,
            status_pid
                | (u32::from(device_addr) << TD_TOKEN_DEVICE_SHIFT)
                | TD_TOKEN_DATA_TOGGLE
                | (encode_len(0) << TD_TOKEN_MAXLEN_SHIFT),
            0,
        );
        let last = slot;

        // Now that every TD's final position is known, patch each link
        // field to point at the next TD (depth-first, so the whole chain
        // is serviced in one frame rather than one TD per frame).
        for i in 0..last {
            let ptr = self.td_addr(i) as *mut RawTd;
            unsafe {
                core::ptr::write_volatile(&raw mut (*ptr).link, self.td_addr(i + 1) | LINK_DEPTH_FIRST);
            }
        }

        self.write_qh(LINK_TERMINATE, self.td_addr(0));

        let deadline_ticks = 50; // 500 ms at 100 Hz -- generous for a control transfer
        let start = interrupts::ticks();
        loop {
            let mut all_done = true;
            let mut error = false;
            for i in 0..=last {
                let control = self.read_td_control(i);
                if control & TD_CTRL_ACTIVE != 0 {
                    all_done = false;
                    break;
                }
                if control & TD_CTRL_ERROR_MASK != 0 {
                    error = true;
                    break;
                }
            }
            if error {
                self.write_qh(LINK_TERMINATE, LINK_TERMINATE);
                return Err(Error::TransferFailed);
            }
            if all_done {
                break;
            }
            if interrupts::ticks().wrapping_sub(start) > deadline_ticks {
                self.write_qh(LINK_TERMINATE, LINK_TERMINATE);
                return Err(Error::Timeout);
            }
        }
        self.write_qh(LINK_TERMINATE, LINK_TERMINATE);

        if data.is_empty() || !data_is_in {
            return Ok(data.len());
        }
        // Actual transferred length is only meaningful on the last data
        // TD that actually ran; a short packet (fewer bytes than
        // requested, e.g. the device's descriptor is shorter than we
        // asked for) ends the data stage early, so sum what each data TD
        // (excluding SETUP at index 0 and STATUS at `last`) really moved.
        let mut total = 0usize;
        for i in 1..last {
            let control = self.read_td_control(i);
            total += decode_actlen(control);
        }
        Ok(total)
    }

    /// Reads `len` bytes of a standard descriptor (`GET_DESCRIPTOR`) into
    /// `out`, from `device_addr`'s endpoint 0.
    pub fn get_descriptor(
        &self,
        device_addr: u8,
        max_packet: u8,
        descriptor_type: u8,
        out: &mut [u8],
    ) -> Result<usize, Error> {
        let len = out.len();
        let setup = [
            0x80, // bmRequestType: device-to-host, standard, device
            USB_REQUEST_GET_DESCRIPTOR,
            0x00,
            descriptor_type,
            0x00,
            0x00,
            (len & 0xff) as u8,
            ((len >> 8) & 0xff) as u8,
        ];
        self.control_transfer(device_addr, max_packet, &setup, out, true)
    }

    pub fn set_address(&self, new_address: u8) -> Result<(), Error> {
        let setup = [0x00, USB_REQUEST_SET_ADDRESS, new_address, 0x00, 0x00, 0x00, 0x00, 0x00];
        self.control_transfer(0, 8, &setup, &mut [], false)?;
        // The device needs a short recovery time after processing
        // SET_ADDRESS before it reliably answers at the new address.
        wait_ticks(1);
        Ok(())
    }

    pub fn set_configuration(&self, device_addr: u8, max_packet: u8, value: u8) -> Result<(), Error> {
        let setup = [0x00, USB_REQUEST_SET_CONFIGURATION, value, 0x00, 0x00, 0x00, 0x00, 0x00];
        self.control_transfer(device_addr, max_packet, &setup, &mut [], false)?;
        Ok(())
    }

    /// Selects one interface's alternate setting -- required before a USB
    /// Audio Class streaming endpoint becomes active, since UAC1 exposes
    /// it only under a non-zero alternate setting (alternate 0 is the
    /// class's own "idle, zero bandwidth" convention).
    pub fn set_interface(
        &self,
        device_addr: u8,
        max_packet: u8,
        interface_number: u8,
        alternate_setting: u8,
    ) -> Result<(), Error> {
        let setup = [
            0x01, // bmRequestType: host-to-device, standard, *interface* recipient
            USB_REQUEST_SET_INTERFACE,
            alternate_setting,
            0x00,
            interface_number,
            0x00,
            0x00,
            0x00,
        ];
        self.control_transfer(device_addr, max_packet, &setup, &mut [], false)?;
        Ok(())
    }

    /// Begins streaming `data_len` bytes of caller-owned PCM data
    /// (already resident at physical address `data_base` -- e.g. the
    /// shared tape staging buffer -- and valid for the whole stream's
    /// duration) as fixed `packet_bytes`-sized isochronous OUT packets
    /// to `endpoint` on `device_addr`. The caller must have already
    /// completed `SET_CONFIGURATION`/`set_interface` for the streaming
    /// alternate setting. Primes the first `ISOC_TD_COUNT` Frame List
    /// entries immediately; `IsocOutStream::service` keeps that window
    /// filled as real time advances.
    pub fn start_isoc_out(
        &self,
        device_addr: u8,
        endpoint: u8,
        packet_bytes: u16,
        data_base: u32,
        data_len: usize,
    ) -> IsocOutStream {
        let start_index =
            (self.read_frame_number() & FRAME_INDEX_MASK).wrapping_add(STARTUP_GUARD_FRAMES) & FRAME_INDEX_MASK;
        let mut stream = IsocOutStream {
            device_addr,
            endpoint,
            packet_bytes,
            data_base,
            data_len,
            next_index: start_index,
            installed_bytes: 0,
            last_pool_slot: 0,
            started_index: start_index,
            entries_written: 0,
            error_count: 0,
        };
        for _ in 0..ISOC_TD_COUNT {
            stream.install_next(self);
        }
        stream
    }
}

#[derive(Debug, Eq, PartialEq)]
pub enum IsocStatus {
    /// The stream still has data to install, or its last installed
    /// packet hasn't finished playing yet.
    Streaming,
    /// Every byte has been installed and the last packet has completed.
    Finished,
}

/// A single active isochronous OUT stream (e.g. USB Audio Class
/// playback), returned by `Uhci::start_isoc_out`. Deliberately a
/// separate type from `Uhci`: every `Uhci` method above is `&self` and
/// self-contained per call, while a stream needs state
/// (`next_index`/`installed_bytes`) that persists across many
/// `service()` calls as real time advances.
///
/// Because the Frame List has exactly `FRAME_LIST_ENTRIES` (1024)
/// entries and hardware walks through all of them once per ~1.024 real
/// seconds, continuous streaming requires refreshing roughly one Frame
/// List entry per real millisecond for as long as the stream runs --
/// unlike a control/bulk queue, where the same Queue Head is simply
/// revisited every frame. `service` keeps a small, continuously
/// advancing window of `ISOC_TD_COUNT` entries primed ahead of
/// hardware's own position (tracked via `USBFRNUM`), refilling entries
/// that have safely fallen behind with the next chunk of PCM data.
pub struct IsocOutStream {
    device_addr: u8,
    endpoint: u8,
    packet_bytes: u16,
    data_base: u32,
    data_len: usize,
    /// Next Frame List index (`0..FRAME_LIST_ENTRIES`) this stream will
    /// install a fresh TD into.
    next_index: u16,
    /// Total bytes already assigned to an installed TD, monotonic
    /// (unlike `next_index`, never wraps) -- both the source of the
    /// next chunk's buffer offset and the "how much is left" check.
    installed_bytes: usize,
    /// The isoc pool slot (`0..ISOC_TD_COUNT`) most recently written, so
    /// `service` can poll specifically that TD's completion once all
    /// data has been installed.
    last_pool_slot: usize,
    /// The very first Frame List index this stream ever wrote into,
    /// remembered so `stop` can restore exactly the entries this stream
    /// touched.
    started_index: u16,
    /// How many Frame List entries this stream has written into in
    /// total (can exceed `FRAME_LIST_ENTRIES` for a stream longer than
    /// ~1 second, since entries are reused cyclically).
    entries_written: u32,
    /// How many completed packets reported an error (`TD_CTRL_ERROR_MASK`
    /// -- STALL/buffer error/babble/NODEV-mapped timeout) rather than a
    /// clean send. Isochronous TDs never retry (`TD_CTRL_IOS`), so a
    /// failed packet is simply lost audio, not something to detect via
    /// `service`'s own `Active`-bit completion check alone -- that bit
    /// clears identically whether a packet succeeded or failed.
    error_count: u32,
}

impl IsocOutStream {
    /// Writes the next chunk of PCM data into one isoc pool TD slot and
    /// points this stream's next Frame List entry at it, advancing
    /// `next_index`/`installed_bytes`/`entries_written`. A no-op once
    /// every byte of `data_len` has already been installed.
    fn install_next(&mut self, uhci: &Uhci) {
        if self.installed_bytes >= self.data_len {
            return;
        }
        let remaining = self.data_len - self.installed_bytes;
        let chunk = remaining.min(self.packet_bytes as usize);
        let pool_slot = (self.entries_written as usize) % ISOC_TD_COUNT;
        // Before reusing this pool slot, check whether the packet it
        // last held reported an error -- isochronous TDs never retry, so
        // this is the only chance to notice a failed packet at all; the
        // `Active` bit `service`'s own completion check polls clears
        // identically whether a packet succeeded or failed.
        if self.entries_written as usize >= ISOC_TD_COUNT
            && uhci.read_isoc_td_control(pool_slot) & TD_CTRL_ERROR_MASK != 0
        {
            self.error_count += 1;
        }
        let buffer_addr = self.data_base + self.installed_bytes as u32;
        uhci.write_isoc_td(
            pool_slot,
            TD_CTRL_ACTIVE | TD_CTRL_IOS,
            PID_OUT
                | (u32::from(self.device_addr) << TD_TOKEN_DEVICE_SHIFT)
                | (u32::from(self.endpoint) << TD_TOKEN_ENDPOINT_SHIFT)
                | (encode_len(chunk) << TD_TOKEN_MAXLEN_SHIFT),
            buffer_addr,
        );
        uhci.write_frame_list_entry(self.next_index, uhci.isoc_td_addr(pool_slot));
        self.last_pool_slot = pool_slot;
        self.installed_bytes += chunk;
        self.next_index = self.next_index.wrapping_add(1) & FRAME_INDEX_MASK;
        self.entries_written += 1;
    }

    /// Keeps this stream's look-ahead window filled, and reports whether
    /// it's done. Call this repeatedly from a foreground polling loop
    /// (see the `usb play` shell command) -- there is no interrupt or
    /// background path driving an `IsocOutStream` on its own.
    pub fn service(&mut self, uhci: &Uhci) -> IsocStatus {
        let current = uhci.read_frame_number() & FRAME_INDEX_MASK;
        while self.installed_bytes < self.data_len
            && (self.next_index.wrapping_sub(current) & FRAME_INDEX_MASK) < ISOC_TD_COUNT as u16
        {
            self.install_next(uhci);
        }
        if self.installed_bytes >= self.data_len
            && uhci.read_isoc_td_control(self.last_pool_slot) & TD_CTRL_ACTIVE == 0
        {
            // The final `ISOC_TD_COUNT` packets are never recycled by
            // `install_next` (the stream ends before their pool slot
            // would be reused), so this is the only chance to notice an
            // error among them.
            for slot in 0..ISOC_TD_COUNT.min(self.entries_written as usize) {
                if uhci.read_isoc_td_control(slot) & TD_CTRL_ERROR_MASK != 0 {
                    self.error_count += 1;
                }
            }
            return IsocStatus::Finished;
        }
        IsocStatus::Streaming
    }

    /// How many completed packets reported an error rather than a clean
    /// send -- see the field's own doc comment. Only meaningful once
    /// `service` has returned `IsocStatus::Finished`.
    pub fn error_count(&self) -> u32 {
        self.error_count
    }

    /// Restores every Frame List entry this stream ever wrote into back
    /// to pointing at the control Queue Head, the same value
    /// `reset_and_start` initializes them to -- so a later control
    /// transfer (e.g. a subsequent `usb`/`usb play` run) sees a clean
    /// schedule again. Call this once, whether the stream finished
    /// normally or was aborted early.
    pub fn stop(&self, uhci: &Uhci) {
        let restore_count = self.entries_written.min(FRAME_LIST_ENTRIES as u32);
        let qh_link = uhci.qh_addr() | LINK_QH_SELECT;
        for i in 0..restore_count {
            let index = self.started_index.wrapping_add(i as u16) & FRAME_INDEX_MASK;
            uhci.write_frame_list_entry(index, qh_link);
        }
    }
}

fn wait_ticks(count: u64) {
    let start = interrupts::ticks();
    while interrupts::ticks().wrapping_sub(start) < count {}
}

static mut CONTROLLERS: [Option<Uhci>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut CONTROLLER_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

fn init_controller(address: PciAddress) -> Result<(), Error> {
    let instance = unsafe { CONTROLLER_COUNT };
    if instance >= MAX_INSTANCES {
        return Err(Error::InitFailed);
    }

    address.enable_io_and_bus_master();
    const UHCI_IO_BAR_INDEX: u8 = 4;
    let Some(io_base) = address.io_bar(UHCI_IO_BAR_INDEX) else {
        return Err(Error::InitFailed);
    };
    let irq = address.interrupt_line();

    let Some(frame_list_addr) = pmem::allocate() else {
        return Err(Error::InitFailed);
    };
    let Some(qh_td_pool_addr) = pmem::allocate() else {
        return Err(Error::InitFailed);
    };
    let Some(isoc_td_pool_addr) = pmem::allocate() else {
        return Err(Error::InitFailed);
    };

    let controller = Uhci {
        instance,
        io_base,
        irq,
        frame_list_addr,
        qh_td_pool_addr,
        isoc_td_pool_addr,
    };
    controller.reset_and_start();

    unsafe {
        CONTROLLERS[instance] = Some(controller);
        CONTROLLER_COUNT += 1;
    }
    Ok(())
}

/// Discovers and starts every usable UHCI controller (up to
/// `MAX_INSTANCES`), mirroring `es1370::initialize_all`/
/// `rtl8139::initialize_all`'s own idempotent, lazily-triggered shape.
pub fn initialize_all() -> Result<usize, Error> {
    unsafe {
        if INITIALIZED {
            return if CONTROLLER_COUNT == 0 {
                Err(Error::NotPresent)
            } else {
                Ok(CONTROLLER_COUNT)
            };
        }
        INITIALIZED = true;
    }

    let mut matches: [Option<PciAddress>; MAX_INSTANCES] = [None; MAX_INSTANCES];
    let found = pci::find_uhci_controllers(&mut matches);
    if found == 0 {
        return Err(Error::NotPresent);
    }

    let mut init_failed = false;
    for address in matches[..found.min(MAX_INSTANCES)].iter().flatten() {
        if init_controller(*address).is_err() {
            init_failed = true;
        }
    }

    unsafe {
        if CONTROLLER_COUNT == 0 {
            if init_failed {
                Err(Error::InitFailed)
            } else {
                Err(Error::NotPresent)
            }
        } else {
            Ok(CONTROLLER_COUNT)
        }
    }
}

pub fn count() -> usize {
    initialize_all().unwrap_or(0)
}

pub fn get(index: usize) -> Option<&'static Uhci> {
    if initialize_all().is_err() {
        return None;
    }
    unsafe {
        if index >= CONTROLLER_COUNT {
            return None;
        }
        CONTROLLERS[index].as_ref()
    }
}
