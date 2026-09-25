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

use core::cell::UnsafeCell;

use crate::arch::x86::{inw, outw};
use crate::audio;
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
// Deliberately no `CMD_GRESET` ("Global Reset") constant: an earlier
// version of `reset_and_start` issued it first, but Global Reset pulses
// a bus reset out to every device on every port of this controller (not
// just a register-level reinit of the controller itself, which
// `CMD_HCRESET` alone already covers). Confirmed on a real, physically
// tested USB-booting Pentium 4 machine to break BIOS's own "USB Legacy
// Support" `INT13h` emulation for the boot device once `usb`/`audio`/
// `usb play`/`usb capture` resets *any* discovered UHCI controller
// (`initialize_all` resets every one found, not just the one about to
// be used) -- and, unlike a normal crash, this is not recoverable by
// `reboot soft` at all, only a genuine hardware reset, since BIOS's own
// broken legacy-USB state is never re-initialized without a real POST.
// Removing Global Reset alone did NOT fix this on that same machine,
// though -- Host Controller Reset alone is already enough to break it.
// See AGENTS.md's "Real hardware notes" section for the full story.

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
/// hardware's own position (`USBFRNUM`) at all times. 250 ms of
/// look-ahead: `usb play`'s own foreground loop calls `service` every
/// spin with no real gap, but `audio.rs`'s `Audio::Uhci` backend is
/// serviced only whenever `poll_background_work` happens to run --
/// e.g. once per received network frame in a tape like `audio-net`, not
/// on a fixed timer -- so this needs real slack against irregular
/// caller cadence and host-scheduling jitter under QEMU, not just
/// "comfortable" margin for uniform overhead. 250 ms of playback latency
/// is imperceptible for this use case (compare a typical network jitter
/// buffer); the alternative (an IRQ-driven guarantee) would need a real
/// isochronous interrupt path, deliberately out of scope here.
const ISOC_TD_COUNT: usize = 250;
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
    /// for isochronous OUT (playback) streaming -- kept entirely separate
    /// from the control pool above, since isochronous TDs are never
    /// linked from a Queue Head and are indexed completely differently (a
    /// small rotating pool driven by `IsocOutStream`, not a bounded
    /// per-call chain).
    isoc_td_pool_addr: u32,
    /// A third, dedicated page of `ISOC_TD_COUNT` Transfer Descriptors
    /// for isochronous IN (capture), kept separate from `isoc_td_pool_addr`
    /// above rather than shared with it: `audio.rs`'s facade keeps a
    /// playback `IsocOutStream` continuously running from the moment any
    /// USB Audio device is discovered, and a discovered capture endpoint's
    /// `IsocInStream` is serviced right alongside it every
    /// `poll_background_work` call -- both streams are genuinely active
    /// at once whenever a device offers both directions (the real
    /// TerraTec card does), so they cannot share one pool's `entries_written
    /// % ISOC_TD_COUNT` slot indexing without one direction's TD writes
    /// silently clobbering the other's.
    isoc_capture_td_pool_addr: u32,
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

    fn isoc_capture_td_addr(&self, slot: usize) -> u32 {
        self.isoc_capture_td_pool_addr + (slot as u32) * 16
    }

    /// The capture-side mirror of `write_isoc_td`, into the dedicated
    /// capture pool (`isoc_capture_td_pool_addr`) instead -- see that
    /// field's own doc comment for why playback and capture cannot share
    /// one pool.
    fn write_isoc_capture_td(&self, slot: usize, control: u32, token: u32, buffer: u32) {
        let ptr = self.isoc_capture_td_addr(slot) as *mut RawTd;
        unsafe {
            core::ptr::write_volatile(&raw mut (*ptr).link, LINK_TERMINATE);
            core::ptr::write_volatile(&raw mut (*ptr).token, token);
            core::ptr::write_volatile(&raw mut (*ptr).buffer, buffer);
            core::ptr::write_volatile(&raw mut (*ptr).control, control);
        }
    }

    fn read_isoc_capture_td_control(&self, slot: usize) -> u32 {
        unsafe { core::ptr::read_volatile((self.isoc_capture_td_addr(slot) + 4) as *const u32) }
    }

    fn write_qh(&self, link: u32, element: u32) {
        let ptr = self.qh_addr() as *mut RawQh;
        unsafe {
            core::ptr::write_volatile(&raw mut (*ptr).link, link);
            core::ptr::write_volatile(&raw mut (*ptr).element, element);
        }
    }

    /// Host Controller Reset, then program the Frame List and start the
    /// controller with every frame pointing at the one control Queue Head
    /// (which starts out empty -- Terminate set). Deliberately no Global
    /// Reset here -- see `CMD_HCRESET`'s neighboring doc comment above for
    /// why: Host Controller Reset alone already puts every register this
    /// function touches back to a known state, so Global Reset was never
    /// needed for this driver's own correctness, only for the (confirmed
    /// harmful, on real "USB Legacy Support" firmware) bus-reset pulse it
    /// also happens to send to every attached device. Dropping it was
    /// re-tested on the same real Pentium 4 machine that found the
    /// original bug and did **not** fix it -- Host Controller Reset alone
    /// is already enough to break that BIOS's legacy `INT13h` emulation,
    /// so there is no gentler register-level reset available to fall back
    /// to. Kept anyway (it does strictly less than before, with no
    /// downside in every QEMU UHCI test), but the underlying "USB audio
    /// and USB-boot media don't reliably coexist" limitation is not
    /// solved by this -- see the "Real hardware notes" section in
    /// AGENTS.md for the full story and the two real machines that
    /// confirmed it.
    fn reset_and_start(&self) {
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
            // `slot` now holds the *next* TD's index -- either another
            // data TD, or (once the loop exits) STATUS. `TD_POOL_CAPACITY`
            // itself (not minus one) is the first genuinely out-of-range
            // index, since valid slots are `0..TD_POOL_CAPACITY`; the
            // previous, off-by-one-stricter bound rejected the last
            // legal STATUS slot outright. Only found by a real device
            // with `bMaxPacketSize0` at the smallest legal full-speed
            // value (8) reading a large configuration descriptor --
            // QEMU's own synthetic device (`max_packet0` 64) never came
            // close to this boundary.
            if slot >= TD_POOL_CAPACITY {
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
    /// filled as real time advances. Finishes once every byte has been
    /// sent and its last packet has completed -- see `usb play`.
    pub fn start_isoc_out(
        &self,
        device_addr: u8,
        endpoint: u8,
        packet_bytes: u16,
        data_base: u32,
        data_len: usize,
    ) -> IsocOutStream {
        self.start_isoc_stream(
            device_addr,
            endpoint,
            packet_bytes,
            IsocOutSource::Buffer { base: data_base, len: data_len },
        )
    }

    /// Begins streaming continuous, open-ended PCM from a network-fed
    /// `PcmRing` (the same ring `Es1370::fill_period`/`Hda`'s own period
    /// refill already drain, via `pop_into`) as fixed `packet_bytes`-sized
    /// isochronous OUT packets to `endpoint` on `device_addr`. Unlike
    /// `start_isoc_out`, this never finishes on its own -- an underrun
    /// (nothing submitted yet, or drained faster than it's filled) is
    /// zero-filled and counted (`IsocOutStream::underrun_count`), the
    /// same "silence, never an error" convention `Es1370::fill_period`
    /// already uses. Allocates a small dedicated packet-copy pool
    /// (`ISOC_TD_COUNT` slots of `packet_bytes` each): unlike
    /// `start_isoc_out`'s zero-copy design, a ring-sourced packet has no
    /// single pre-existing memory location a TD can point at directly,
    /// so each pool slot is a fixed, reused copy destination. Returns
    /// `None` only if that allocation fails.
    pub fn start_isoc_ring_out(
        &self,
        device_addr: u8,
        endpoint: u8,
        packet_bytes: u16,
        ring: &'static audio::PcmRing,
    ) -> Option<IsocOutStream> {
        let pool_bytes = ISOC_TD_COUNT * packet_bytes as usize;
        let frames = (pool_bytes as u32).div_ceil(pmem::FRAME_SIZE) as usize;
        let packet_pool_addr = pmem::allocate_contiguous(frames)?;
        Some(self.start_isoc_stream(
            device_addr,
            endpoint,
            packet_bytes,
            IsocOutSource::Ring { ring, packet_pool_addr },
        ))
    }

    fn start_isoc_stream(&self, device_addr: u8, endpoint: u8, packet_bytes: u16, source: IsocOutSource) -> IsocOutStream {
        let start_index =
            (self.read_frame_number() & FRAME_INDEX_MASK).wrapping_add(STARTUP_GUARD_FRAMES) & FRAME_INDEX_MASK;
        let mut stream = IsocOutStream {
            device_addr,
            endpoint,
            packet_bytes,
            source,
            next_index: start_index,
            installed_bytes: 0,
            last_pool_slot: 0,
            started_index: start_index,
            entries_written: 0,
            error_count: 0,
            underrun_count: 0,
        };
        for _ in 0..ISOC_TD_COUNT {
            stream.install_next(self);
        }
        stream
    }

    /// Begins capturing up to `capacity` bytes of isochronous IN data
    /// from `endpoint` on `device_addr` into caller-owned physical
    /// memory at `base` (e.g. the shared tape staging buffer -- must
    /// stay valid and unused by anything else for the capture's whole
    /// duration), zero-copy exactly like `start_isoc_out`'s own `Buffer`
    /// source. The caller must have already completed
    /// `SET_CONFIGURATION`/`set_interface` for the capture alternate
    /// setting. Primes the first `ISOC_TD_COUNT` Frame List entries
    /// immediately; `IsocInStream::service` keeps that window filled and
    /// harvests completed receive TDs as real time advances.
    pub fn start_isoc_in(&self, device_addr: u8, endpoint: u8, packet_bytes: u16, base: u32, capacity: usize) -> IsocInStream {
        self.start_isoc_in_stream(device_addr, endpoint, packet_bytes, IsocInSink::Buffer { base, capacity })
    }

    /// Begins capturing continuous, open-ended PCM from `endpoint` on
    /// `device_addr`, pushing each received packet into a network-fed
    /// `PcmRing` (the same ring `Es1370::drain_capture_period`/`Hda`'s own
    /// capture path already fill, via `push`) -- `audio.rs`'s USB capture
    /// backend. Unlike `start_isoc_in`, this never finishes on its own; a
    /// short push (the ring already has an unread backlog) is counted
    /// (`IsocInStream::overrun_count`), the capture-side counterpart to
    /// `IsocOutStream::underrun_count`. Allocates a small dedicated
    /// packet-copy pool (`ISOC_TD_COUNT` slots of `packet_bytes` each),
    /// exactly like `start_isoc_ring_out`'s own reasoning: a ring has no
    /// single fixed memory location for hardware to write a received
    /// packet directly into. Returns `None` only if that allocation fails.
    pub fn start_isoc_ring_in(
        &self,
        device_addr: u8,
        endpoint: u8,
        packet_bytes: u16,
        ring: &'static audio::PcmRing,
    ) -> Option<IsocInStream> {
        let pool_bytes = ISOC_TD_COUNT * packet_bytes as usize;
        let frames = (pool_bytes as u32).div_ceil(pmem::FRAME_SIZE) as usize;
        let packet_pool_addr = pmem::allocate_contiguous(frames)?;
        Some(self.start_isoc_in_stream(device_addr, endpoint, packet_bytes, IsocInSink::Ring { ring, packet_pool_addr }))
    }

    fn start_isoc_in_stream(&self, device_addr: u8, endpoint: u8, packet_bytes: u16, sink: IsocInSink) -> IsocInStream {
        let start_index =
            (self.read_frame_number() & FRAME_INDEX_MASK).wrapping_add(STARTUP_GUARD_FRAMES) & FRAME_INDEX_MASK;
        let mut stream = IsocInStream {
            device_addr,
            endpoint,
            packet_bytes,
            sink,
            next_index: start_index,
            requested_bytes: 0,
            received_bytes: 0,
            last_pool_slot: 0,
            started_index: start_index,
            entries_written: 0,
            error_count: 0,
            overrun_count: 0,
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

/// Where an `IsocOutStream`'s packet bytes come from. `Buffer` is
/// `usb play`'s own use case: a fixed, pre-loaded region of known
/// length, zero-copy (the TD points directly into it), finishing once
/// every byte has been sent. `Ring` is `audio.rs`'s USB playback
/// backend: continuous, open-ended, network-fed data via the same
/// `PcmRing` `Es1370`/`Hda` already drain, needing a small dedicated
/// packet-copy pool (a ring-sourced packet has no single pre-existing
/// memory location to point a TD at directly) and never finishing on
/// its own.
enum IsocOutSource {
    Buffer { base: u32, len: usize },
    Ring { ring: &'static audio::PcmRing, packet_pool_addr: u32 },
}

/// A single active isochronous OUT stream (e.g. USB Audio Class
/// playback), returned by `Uhci::start_isoc_out`/`start_isoc_ring_out`.
/// Deliberately a separate type from `Uhci`: every `Uhci` method above
/// is `&self` and self-contained per call, while a stream needs state
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
/// that have safely fallen behind with the next chunk of PCM data --
/// this priming/window-management machinery is shared and identical
/// regardless of `source`; only "where do this packet's bytes come
/// from" (`install_next`) and "when is this stream done" (`service`)
/// vary by source.
pub struct IsocOutStream {
    device_addr: u8,
    endpoint: u8,
    packet_bytes: u16,
    source: IsocOutSource,
    /// Next Frame List index (`0..FRAME_LIST_ENTRIES`) this stream will
    /// install a fresh TD into.
    next_index: u16,
    /// Total bytes ever assigned to an installed TD, monotonic (unlike
    /// `next_index`, never wraps). For `Buffer`, this is also the "how
    /// much is left" check and the next chunk's offset into `base`; for
    /// `Ring`, it is purely a diagnostic running total.
    installed_bytes: usize,
    /// The isoc pool slot (`0..ISOC_TD_COUNT`) most recently written, so
    /// `service` can poll specifically that TD's completion once a
    /// `Buffer` stream has installed every byte.
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
    /// How many packets, for a `Ring`-sourced stream, found the ring
    /// empty and were sent as silence instead -- the isochronous-OUT
    /// counterpart to `Es1370`'s own `underrun_count`. Always 0 for a
    /// `Buffer`-sourced stream (there is no ring to underrun).
    underrun_count: u32,
}

impl IsocOutStream {
    /// Whether a `Buffer`-sourced stream has installed every byte of
    /// `len` already -- always `false` for `Ring`, which never finishes
    /// on its own.
    fn buffer_exhausted(&self) -> bool {
        matches!(self.source, IsocOutSource::Buffer { len, .. } if self.installed_bytes >= len)
    }

    /// Writes the next packet into one isoc pool TD slot and points this
    /// stream's next Frame List entry at it, advancing
    /// `next_index`/`installed_bytes`/`entries_written`. A no-op once a
    /// `Buffer` stream has already installed every byte of `len`.
    fn install_next(&mut self, uhci: &Uhci) {
        if self.buffer_exhausted() {
            return;
        }
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

        let (chunk, buffer_addr) = match self.source {
            IsocOutSource::Buffer { base, len } => {
                let chunk = (len - self.installed_bytes).min(self.packet_bytes as usize);
                (chunk, base + self.installed_bytes as u32)
            }
            IsocOutSource::Ring { ring, packet_pool_addr } => {
                let chunk = self.packet_bytes as usize;
                let buffer_addr = packet_pool_addr + (pool_slot as u32) * u32::from(self.packet_bytes);
                // Mirrors `Es1370::fill_period`'s own "pull a full
                // period from the ring, falling back to silence (never
                // an error) on an underrun" convention.
                let filled = unsafe { ring.pop_into(buffer_addr as *mut u8, chunk) };
                if !filled {
                    self.underrun_count += 1;
                    unsafe { core::ptr::write_bytes(buffer_addr as *mut u8, 0, chunk) };
                }
                (chunk, buffer_addr)
            }
        };

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
    /// it's done -- always `IsocStatus::Streaming` for a `Ring`-sourced
    /// stream, which never finishes on its own. Call this repeatedly
    /// from a foreground polling loop (`usb play`, or `audio.rs`'s
    /// `poll_background_work` hook) -- there is no interrupt or other
    /// background path driving an `IsocOutStream` on its own.
    pub fn service(&mut self, uhci: &Uhci) -> IsocStatus {
        let current = uhci.read_frame_number() & FRAME_INDEX_MASK;

        // How many frames `next_index` currently sits ahead of hardware's
        // real position, as a *signed* quantity: `next_index.wrapping_sub
        // (current) & FRAME_INDEX_MASK` alone is unsigned modulo 1024, so
        // "3 frames behind" (a value of 1021) is indistinguishable from
        // "1021 frames ahead" without this sign correction -- a caller
        // that goes a while between `service()` calls (e.g. a tape's own
        // receive loop paced by how often network frames actually
        // arrive, not a fixed timer) previously looked *already caught
        // up* by this same miscalculation and silently primed nothing at
        // all, permanently starving the ring from that point on.
        let raw_gap = self.next_index.wrapping_sub(current) & FRAME_INDEX_MASK;
        let half = (FRAME_LIST_ENTRIES / 2) as i32;
        let mut ahead = raw_gap as i32;
        if ahead > half {
            ahead -= FRAME_LIST_ENTRIES as i32;
        }
        if ahead < 0 {
            // Genuinely fallen behind: the frame list slots between the
            // old schedule and hardware's current position are
            // unrecoverable (already serviced one way or another, most
            // likely with a stale, already-`Active`-cleared TD played as
            // silence). Resync a few frames ahead of *now* rather than
            // mechanically replaying a backlog into slots that would
            // otherwise only play a full lap (~1s) from now.
            self.next_index = current.wrapping_add(STARTUP_GUARD_FRAMES) & FRAME_INDEX_MASK;
        }

        while !self.buffer_exhausted() {
            let raw_gap = self.next_index.wrapping_sub(current) & FRAME_INDEX_MASK;
            let ahead = if raw_gap as i32 > half { raw_gap as i32 - FRAME_LIST_ENTRIES as i32 } else { raw_gap as i32 };
            if ahead >= ISOC_TD_COUNT as i32 {
                break;
            }
            self.install_next(uhci);
        }
        if self.buffer_exhausted() && uhci.read_isoc_td_control(self.last_pool_slot) & TD_CTRL_ACTIVE == 0 {
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
    /// `service` has returned `IsocStatus::Finished` for a `Buffer`
    /// stream, or at any time for a `Ring` stream (which never finishes).
    pub fn error_count(&self) -> u32 {
        self.error_count
    }

    /// How many packets were sent as silence because a `Ring`-sourced
    /// stream's ring was empty -- see the field's own doc comment.
    pub fn underrun_count(&self) -> u32 {
        self.underrun_count
    }

    /// How many Frame List entries this stream has ever installed --
    /// the `periods_serviced` analog `UsbAudioDevice::interrupt_stats`
    /// reports, since there is no interrupt to count here at all.
    pub fn entries_written(&self) -> u32 {
        self.entries_written
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

/// Where an `IsocInStream`'s received packet bytes go. `Buffer` is
/// `usb capture`'s own use case: a fixed, pre-allocated region of known
/// capacity, zero-copy (the TD's buffer pointer is `base +
/// requested_bytes`, so hardware writes samples straight into their
/// final position), finishing once every requested byte's TD has
/// completed. `Ring` is `audio.rs`'s USB capture backend: continuous,
/// open-ended data pushed into the same `PcmRing`
/// `Es1370::drain_capture_period`/`Hda`'s own capture path already fill,
/// via a small dedicated packet-copy pool (a receive TD needs *some*
/// fixed physical destination to write into; a ring has no single one),
/// and never finishing on its own.
enum IsocInSink {
    Buffer { base: u32, capacity: usize },
    Ring { ring: &'static audio::PcmRing, packet_pool_addr: u32 },
}

/// A single active isochronous IN stream (USB Audio Class capture),
/// returned by `Uhci::start_isoc_in`/`start_isoc_ring_in`. The mirror
/// image of `IsocOutStream` -- reusing the identical Frame-List-window-
/// priming machinery, and, since `audio.rs`'s facade can have a playback
/// `IsocOutStream` and a capture `IsocInStream` both genuinely active at
/// once (see `Uhci::isoc_capture_td_pool_addr`'s own doc comment), backed
/// by its *own* dedicated TD pool rather than sharing the OUT direction's
/// -- but with the data flow reversed in a way that changes exactly one
/// thing structurally: for OUT, the driver already knows each packet's
/// exact content before installing its TD; for IN, hardware decides how
/// many bytes it actually sends (`ActLen`, read back via `decode_actlen`
/// once `Active` clears), so a completed TD's data must be *harvested* --
/// read out and folded into `sink` -- before its pool slot is reused for
/// a fresh receive TD. A short packet (`ActLen < packet_bytes`, legitimate
/// under Adaptive sync) simply harvests fewer bytes than requested; for a
/// `Buffer` sink this leaves whatever was already at the unwritten tail of
/// that slot -- an accepted imprecision for a raw diagnostic capture, not
/// a byte-exact protocol.
pub struct IsocInStream {
    device_addr: u8,
    endpoint: u8,
    packet_bytes: u16,
    sink: IsocInSink,
    next_index: u16,
    /// Upper-bound bytes ever requested (each installed TD's `MaxLen`
    /// summed) -- known immediately at install time, the "should I
    /// request more" check for a `Buffer` sink, exactly like
    /// `IsocOutStream`'s `installed_bytes` for its `Buffer` source.
    /// Purely a diagnostic running total for `Ring`, which never stops
    /// requesting.
    requested_bytes: usize,
    /// Bytes actually confirmed received so far (summed `ActLen` across
    /// every harvested TD) -- only known after a full round trip, and
    /// the real count `usb capture` reports at the end.
    received_bytes: usize,
    last_pool_slot: usize,
    started_index: u16,
    entries_written: u32,
    error_count: u32,
    /// How many harvested packets, for a `Ring`-sourced stream, found the
    /// ring already full and had some or all of their bytes dropped --
    /// the isochronous-IN counterpart to `IsocOutStream::underrun_count`,
    /// mirroring `Es1370::drain_capture_period`'s identical "short push ->
    /// overrun" accounting. Always 0 for a `Buffer`-sourced stream (there
    /// is no ring to overrun).
    overrun_count: u32,
}

impl IsocInStream {
    /// Whether a `Buffer`-sourced stream has requested every byte of
    /// `capacity` already -- always `false` for `Ring`, which never
    /// finishes on its own.
    fn request_exhausted(&self) -> bool {
        matches!(self.sink, IsocInSink::Buffer { capacity, .. } if self.requested_bytes >= capacity)
    }

    /// Reads back one already-completed receive TD's actual length and
    /// error status, folding it into `received_bytes`/`error_count` and,
    /// for a `Ring` sink, copying the received bytes out of that slot's
    /// fixed scratch-pool position into the ring (counting
    /// `overrun_count` on a short push). Called for pool slot `slot`
    /// exactly once per TD ever installed there -- either just before
    /// `install_next` reuses that slot for a fresh receive TD, or, for
    /// the final un-recycled batch, from `service`'s own tail scan once
    /// capture stops (mirroring `IsocOutStream::service`'s identical
    /// tail-scan reasoning for errors: every pool slot holds exactly the
    /// last TD ever written to it, and any slot reused earlier was
    /// already harvested at that reuse point, so scanning every slot
    /// once at the end is correct and never double-counts).
    fn harvest(&mut self, uhci: &Uhci, slot: usize) {
        let control = uhci.read_isoc_capture_td_control(slot);
        if control & TD_CTRL_ERROR_MASK != 0 {
            self.error_count += 1;
            return;
        }
        let actual = decode_actlen(control);
        if let IsocInSink::Ring { ring, packet_pool_addr } = self.sink {
            let buffer_addr = packet_pool_addr + (slot as u32) * u32::from(self.packet_bytes);
            let received = unsafe { core::slice::from_raw_parts(buffer_addr as *const u8, actual) };
            if ring.push(received) < actual {
                self.overrun_count += 1;
            }
        }
        self.received_bytes += actual;
    }

    /// Harvests the previous occupant of the next pool slot (if any),
    /// then writes a fresh receive TD there and points this stream's
    /// next Frame List entry at it. A no-op once a `Buffer` stream's
    /// `requested_bytes` has already reached `capacity`.
    fn install_next(&mut self, uhci: &Uhci) {
        if self.request_exhausted() {
            return;
        }
        let pool_slot = (self.entries_written as usize) % ISOC_TD_COUNT;
        if self.entries_written as usize >= ISOC_TD_COUNT {
            self.harvest(uhci, pool_slot);
        }
        let (chunk, buffer_addr) = match self.sink {
            IsocInSink::Buffer { base, capacity } => {
                let chunk = (capacity - self.requested_bytes).min(self.packet_bytes as usize);
                (chunk, base + self.requested_bytes as u32)
            }
            IsocInSink::Ring { packet_pool_addr, .. } => {
                let chunk = self.packet_bytes as usize;
                (chunk, packet_pool_addr + (pool_slot as u32) * u32::from(self.packet_bytes))
            }
        };
        uhci.write_isoc_capture_td(
            pool_slot,
            TD_CTRL_ACTIVE | TD_CTRL_IOS,
            PID_IN
                | (u32::from(self.device_addr) << TD_TOKEN_DEVICE_SHIFT)
                | (u32::from(self.endpoint) << TD_TOKEN_ENDPOINT_SHIFT)
                | (encode_len(chunk) << TD_TOKEN_MAXLEN_SHIFT),
            buffer_addr,
        );
        uhci.write_frame_list_entry(self.next_index, uhci.isoc_capture_td_addr(pool_slot));
        self.last_pool_slot = pool_slot;
        self.requested_bytes += chunk;
        self.next_index = self.next_index.wrapping_add(1) & FRAME_INDEX_MASK;
        self.entries_written += 1;
    }

    /// Keeps this stream's look-ahead window filled, and reports whether
    /// it's done -- always `IsocStatus::Streaming` for a `Ring`-sourced
    /// stream, which never finishes on its own. See
    /// `IsocOutStream::service`'s own doc comment for why the
    /// window-tracking math must use a signed gap (a caller that goes a
    /// while between calls must resync a few frames ahead of *now*, not
    /// silently stall forever). Call this repeatedly from a foreground
    /// polling loop (`usb capture`, or `audio.rs`'s `poll_background_work`
    /// hook) -- there is no interrupt driving an `IsocInStream` on its
    /// own.
    pub fn service(&mut self, uhci: &Uhci) -> IsocStatus {
        let current = uhci.read_frame_number() & FRAME_INDEX_MASK;

        let raw_gap = self.next_index.wrapping_sub(current) & FRAME_INDEX_MASK;
        let half = (FRAME_LIST_ENTRIES / 2) as i32;
        let mut ahead = raw_gap as i32;
        if ahead > half {
            ahead -= FRAME_LIST_ENTRIES as i32;
        }
        if ahead < 0 {
            self.next_index = current.wrapping_add(STARTUP_GUARD_FRAMES) & FRAME_INDEX_MASK;
        }

        while !self.request_exhausted() {
            let raw_gap = self.next_index.wrapping_sub(current) & FRAME_INDEX_MASK;
            let ahead = if raw_gap as i32 > half { raw_gap as i32 - FRAME_LIST_ENTRIES as i32 } else { raw_gap as i32 };
            if ahead >= ISOC_TD_COUNT as i32 {
                break;
            }
            self.install_next(uhci);
        }
        if self.request_exhausted() && uhci.read_isoc_capture_td_control(self.last_pool_slot) & TD_CTRL_ACTIVE == 0 {
            // The final `ISOC_TD_COUNT` receive TDs are never recycled by
            // `install_next` (capture stops before their pool slot would
            // be reused), so this is the only chance to harvest them.
            for slot in 0..ISOC_TD_COUNT.min(self.entries_written as usize) {
                self.harvest(uhci, slot);
            }
            return IsocStatus::Finished;
        }
        IsocStatus::Streaming
    }

    pub fn error_count(&self) -> u32 {
        self.error_count
    }

    /// How many bytes have actually been confirmed received (harvested)
    /// so far -- the real count `usb capture` reports, since a short
    /// final packet or an error means this can be less than `capacity`.
    pub fn received_bytes(&self) -> usize {
        self.received_bytes
    }

    /// How many harvested packets, for a `Ring`-sourced stream, found the
    /// ring already full -- see the field's own doc comment.
    pub fn overrun_count(&self) -> u32 {
        self.overrun_count
    }

    /// How many Frame List entries this stream has ever installed -- the
    /// `periods_serviced`/`entries_written` analog `UsbAudioDevice`
    /// reports for capture, mirroring `IsocOutStream::entries_written`.
    pub fn entries_written(&self) -> u32 {
        self.entries_written
    }

    /// Restores every Frame List entry this stream ever wrote into back
    /// to the control Queue Head -- identical to
    /// `IsocOutStream::stop`'s own reasoning.
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
    let Some(isoc_capture_td_pool_addr) = pmem::allocate() else {
        return Err(Error::InitFailed);
    };

    let controller = Uhci {
        instance,
        io_base,
        irq,
        frame_list_addr,
        qh_td_pool_addr,
        isoc_td_pool_addr,
        isoc_capture_td_pool_addr,
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

/// A USB Audio Class 1.0 endpoint this driver can actually drive, either
/// direction: PCM, 16-bit, a single fixed discrete sample rate divisible
/// by 1000 (so every isochronous packet is the same fixed size for the
/// whole stream -- a device like a 44100 Hz one, which would need a
/// varying packet size frame-to-frame, is deliberately out of scope; see
/// AGENTS.md's "UHCI" section), and a Synchronous or Adaptive (not
/// Asynchronous, which would need an unimplemented feedback endpoint)
/// isochronous endpoint whose declared `wMaxPacketSize` is at least this
/// nominal per-frame byte count. `packet_bytes` is always that computed
/// *nominal* count, never the endpoint's own declared maximum: a real
/// Adaptive-sync device (confirmed against a genuine USB Audio Class 1.0
/// sound card, not just QEMU's Synchronous-only emulated one)
/// legitimately declares some slack above nominal to tolerate a sample
/// or so of clock drift per frame against its own crystal, but this
/// driver always sends/expects exactly the nominal size regardless --
/// correct and sufficient for a short stream, where that drift is
/// negligible; real feedback-based rate adaptation for a long-running
/// Adaptive stream remains explicitly out of scope, the same limit
/// already covering Asynchronous sync devices outright.
#[derive(Clone, Copy)]
pub struct UacEndpoint {
    pub interface_number: u8,
    pub alternate_setting: u8,
    pub endpoint_address: u8,
    pub packet_bytes: u16,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
}

/// Both directions a UAC1 device might offer, discovered together from
/// one `find_uac_device` pass -- a real device's playback and capture
/// interfaces (e.g. a stereo speaker output alongside a mono microphone
/// input) can have completely different formats, so callers must ask
/// for whichever direction they need rather than assuming one shared
/// format the way `es1370`/`hda`'s own real hardware happens to have.
pub struct UacDevice {
    pub playback: Option<UacEndpoint>,
    pub capture: Option<UacEndpoint>,
}

/// Walks a configuration descriptor's raw bytes generically, one
/// sub-descriptor at a time (each one's own `bLength` at byte 0),
/// looking for one AudioStreaming alternate setting with a working
/// isochronous endpoint in the requested direction (`want_in`: `false`
/// for an OUT/playback endpoint, `true` for an IN/capture one). See
/// `UacEndpoint`'s own doc comment for exactly which UAC1 shapes
/// qualify; anything else (a continuous sample-rate range, Asynchronous
/// sync, non-16-bit, the wrong direction, or a declared packet size too
/// small for the computed nominal one) is skipped rather than guessed
/// at.
fn find_uac_endpoint(config: &[u8], want_in: bool) -> Option<UacEndpoint> {
    const DESC_INTERFACE: u8 = 0x04;
    const DESC_CS_INTERFACE: u8 = 0x24;
    const DESC_ENDPOINT: u8 = 0x05;
    const SUBTYPE_AS_GENERAL: u8 = 0x01;
    const SUBTYPE_FORMAT_TYPE: u8 = 0x02;
    const CLASS_AUDIO: u8 = 0x01;
    const SUBCLASS_AUDIOSTREAMING: u8 = 0x02;

    let mut offset = 0usize;
    let mut interface_number = 0u8;
    let mut alternate_setting = 0u8;
    let mut in_streaming_interface = false;
    let mut format_ok = false;
    let mut sample_rate = 0u32;
    let mut channels = 0u8;
    let mut bits_per_sample = 0u8;

    while offset + 1 < config.len() {
        let length = config[offset] as usize;
        if length < 2 || offset + length > config.len() {
            break;
        }
        let descriptor_type = config[offset + 1];
        match descriptor_type {
            DESC_INTERFACE if length >= 9 => {
                interface_number = config[offset + 2];
                alternate_setting = config[offset + 3];
                let num_endpoints = config[offset + 4];
                let class = config[offset + 5];
                let subclass = config[offset + 6];
                // UAC1's own convention: an AudioStreaming interface's
                // alternate setting 0 always declares zero endpoints
                // ("idle"/zero-bandwidth); this naturally skips it and
                // only considers an alternate setting with a real
                // streaming endpoint.
                in_streaming_interface =
                    class == CLASS_AUDIO && subclass == SUBCLASS_AUDIOSTREAMING && num_endpoints == 1;
                format_ok = false;
            }
            DESC_CS_INTERFACE if in_streaming_interface && length >= 3 => {
                let subtype = config[offset + 2];
                if subtype == SUBTYPE_AS_GENERAL && length >= 7 {
                    let format_tag = u16::from_le_bytes([config[offset + 5], config[offset + 6]]);
                    format_ok = format_tag == 1; // PCM
                } else if subtype == SUBTYPE_FORMAT_TYPE && format_ok && length >= 8 {
                    let format_type = config[offset + 3];
                    let nr_channels = config[offset + 4];
                    let bit_resolution = config[offset + 6];
                    let sam_freq_type = config[offset + 7];
                    if format_type == 1 && bit_resolution == 16 && sam_freq_type >= 1 && length >= 11 {
                        // tSamFreq[0]: the first (and, for this plan's
                        // scope, only usable) discrete sample rate --
                        // continuous-range devices (`bSamFreqType == 0`)
                        // are rejected above.
                        sample_rate = u32::from(config[offset + 8])
                            | (u32::from(config[offset + 9]) << 8)
                            | (u32::from(config[offset + 10]) << 16);
                        channels = nr_channels;
                        bits_per_sample = bit_resolution;
                    } else {
                        format_ok = false;
                    }
                }
            }
            DESC_ENDPOINT if in_streaming_interface && format_ok && length >= 7 => {
                let endpoint_address = config[offset + 2];
                let attributes = config[offset + 3];
                let max_packet_size = u16::from_le_bytes([config[offset + 4], config[offset + 5]]);
                let transfer_type = attributes & 0x03;
                let sync_type = (attributes >> 2) & 0x03;
                let is_in = endpoint_address & 0x80 != 0;
                let is_isochronous = transfer_type == 0x01;
                let sync_supported = sync_type == 0x02 || sync_type == 0x03; // Adaptive|Synchronous
                let nominal_bytes =
                    (sample_rate / 1000) * u32::from(channels) * (u32::from(bits_per_sample) / 8);
                // The endpoint must declare *at least* room for our
                // fixed-cadence nominal packet -- exact equality (QEMU's
                // Synchronous device) and real slack above it (a real
                // Adaptive device) both qualify; too little room does not.
                let usable_packet_size = sample_rate % 1000 == 0 && u32::from(max_packet_size) >= nominal_bytes;
                if is_isochronous && is_in == want_in && sync_supported && usable_packet_size {
                    return Some(UacEndpoint {
                        interface_number,
                        alternate_setting,
                        endpoint_address,
                        packet_bytes: nominal_bytes as u16,
                        sample_rate,
                        channels,
                        bits_per_sample,
                    });
                }
            }
            _ => {}
        }
        offset += length;
    }
    None
}

/// Scans every discovered controller/port for a device offering a
/// usable playback and/or capture endpoint (`find_uac_endpoint`,
/// each direction independent -- a device may have only one, or, like
/// this project's real TerraTec test card, both), completing
/// `SET_ADDRESS`/`SET_CONFIGURATION` and `set_interface` for whichever
/// direction(s) were actually found, all from *one* port
/// reset/configuration pass. Both directions are discovered together
/// deliberately, not via two independent calls: this project found,
/// empirically, that two full resets of the same real device back to
/// back with no settle time between them can intermittently fail (see
/// AGENTS.md's "UHCI" section) -- unifying discovery into one pass avoids
/// ever risking that within a single boot. Mirrors
/// `enumerate_usb_port`'s own "reset and redo everything from scratch"
/// sequence rather than depending on a separate, prior `usb` call having
/// already addressed the device. Shared by the `usb play`/`usb capture`
/// shell commands and `audio.rs`'s USB playback discovery.
pub fn find_uac_device() -> Option<(&'static Uhci, UacDevice)> {
    const DEVICE_ADDRESS: u8 = 1;

    for index in 0..count() {
        let Some(controller) = get(index) else { continue };
        for port in 0..UHCI_PORTS {
            let Ok(low_speed) = controller.reset_port(port) else { continue };
            if low_speed {
                continue;
            }
            let mut first8 = [0u8; 8];
            if controller.get_descriptor(0, 8, USB_DESCRIPTOR_DEVICE, &mut first8).is_err() {
                continue;
            }
            let max_packet0 = first8[7];
            if controller.set_address(DEVICE_ADDRESS).is_err() {
                continue;
            }
            let mut config_header = [0u8; 9];
            if controller
                .get_descriptor(DEVICE_ADDRESS, max_packet0, USB_DESCRIPTOR_CONFIGURATION, &mut config_header)
                .is_err()
            {
                continue;
            }
            let total_length =
                (u16::from_le_bytes([config_header[2], config_header[3]]) as usize).min(MAX_DESCRIPTOR_BYTES);
            let mut config = [0u8; MAX_DESCRIPTOR_BYTES];
            let Ok(read) = controller.get_descriptor(
                DEVICE_ADDRESS,
                max_packet0,
                USB_DESCRIPTOR_CONFIGURATION,
                &mut config[..total_length],
            ) else {
                continue;
            };
            let playback = find_uac_endpoint(&config[..read], false);
            let capture = find_uac_endpoint(&config[..read], true);
            if playback.is_none() && capture.is_none() {
                continue;
            }
            // bConfigurationValue is the configuration descriptor's 6th byte.
            let config_value = config.get(5).copied().unwrap_or(0);
            if controller.set_configuration(DEVICE_ADDRESS, max_packet0, config_value).is_err() {
                continue;
            }
            let playback = playback.filter(|endpoint| {
                controller
                    .set_interface(DEVICE_ADDRESS, max_packet0, endpoint.interface_number, endpoint.alternate_setting)
                    .is_ok()
            });
            let capture = capture.filter(|endpoint| {
                controller
                    .set_interface(DEVICE_ADDRESS, max_packet0, endpoint.interface_number, endpoint.alternate_setting)
                    .is_ok()
            });
            if playback.is_none() && capture.is_none() {
                continue;
            }
            return Some((controller, UacDevice { playback, capture }));
        }
    }
    None
}

/// The `audio.rs`-facing wrapper around one discovered, already-`SET_
/// INTERFACE`d USB Audio Class device: an `IsocOutStream` continuously
/// primed from a dedicated `PcmRing` for playback (always present --
/// `discover_usb_audio_device` requires a playback endpoint), plus an
/// optional `IsocInStream` for capture (`None` when the device has no
/// microphone interface at all), and the endpoint metadata needed to
/// answer `format()`/`capture_format()`. `UnsafeCell` interior mutability
/// mirrors `PcmRing`'s own established "single-threaded, no real
/// locking, `unsafe` mutation through a shared reference" pattern
/// (`audio.rs`'s own safety-argument doc comment already justifies this
/// once; this type relies on the identical reasoning, since only ever
/// one foreground command/tape drives this kernel at a time).
pub struct UsbAudioDevice {
    controller: &'static Uhci,
    endpoint: UacEndpoint,
    ring: &'static audio::PcmRing,
    stream: UnsafeCell<IsocOutStream>,
    capture_endpoint: Option<UacEndpoint>,
    capture_stream: UnsafeCell<Option<IsocInStream>>,
}

// Safety: see the struct's own doc comment -- exactly the same
// single-core, no-concurrent-access argument `PcmRing`'s identical impl
// already relies on.
unsafe impl Sync for UsbAudioDevice {}

impl UsbAudioDevice {
    fn stream(&self) -> &mut IsocOutStream {
        unsafe { &mut *self.stream.get() }
    }

    fn capture_stream(&self) -> Option<&mut IsocInStream> {
        unsafe { (*self.capture_stream.get()).as_mut() }
    }

    pub fn driver_name(&self) -> &'static str {
        "USB Audio"
    }

    pub fn format(&self) -> audio::AudioFormat {
        audio::AudioFormat {
            sample_rate: self.endpoint.sample_rate,
            channels: self.endpoint.channels,
            bits_per_sample: self.endpoint.bits_per_sample,
        }
    }

    /// The capture direction's own format -- genuinely independent of
    /// `format()`'s playback numbers on real hardware (the TerraTec
    /// Aureon Dual's capture interface is mono against a stereo playback
    /// format), unlike ES1370/HDA where one shared format legitimately
    /// describes both directions. Falls back to the playback format when
    /// no capture endpoint was ever discovered -- a harmless default,
    /// since `capture`/`capture_backlog` already report zero bytes in
    /// that case regardless of what format a caller thinks it's reading.
    pub fn capture_format(&self) -> audio::AudioFormat {
        let endpoint = self.capture_endpoint.as_ref().unwrap_or(&self.endpoint);
        audio::AudioFormat {
            sample_rate: endpoint.sample_rate,
            channels: endpoint.channels,
            bits_per_sample: endpoint.bits_per_sample,
        }
    }

    /// Queues PCM bytes for playback -- see `Es1370::submit`'s identical
    /// contract. Unlike ES1370/HDA, this device has no "test tone before
    /// the first submit" mode: it is continuously primed with silence
    /// (via `IsocOutSource::Ring`'s own underrun handling) from the
    /// moment it's discovered, so there is no separate mode to switch
    /// out of here.
    pub fn submit(&self, data: &[u8]) -> usize {
        self.ring.push(data)
    }

    pub fn underrun_count(&self) -> u32 {
        self.stream().underrun_count()
    }

    pub fn ring_backlog(&self) -> usize {
        self.ring.available()
    }

    /// No interrupt exists for this backend at all -- every isochronous
    /// packet is sent from foreground polling (`service_active_audio`,
    /// via `poll_background_work`), so `irq`/`interrupt_count` are
    /// always `None`/`0`. `entries_written` (one Frame List entry per
    /// packet) is the closest analog to `periods_serviced`.
    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32) {
        (None, 0, self.stream().entries_written())
    }

    pub fn io_base(&self) -> u32 {
        u32::from(self.controller.io_base())
    }

    /// No hardware status register exists for one isochronous stream,
    /// unlike ES1370/HDA's own memory-mapped register -- an honest zero,
    /// not a fabricated value.
    pub fn raw_status(&self) -> u32 {
        0
    }

    /// Copies out whatever captured PCM bytes are currently available --
    /// see `Es1370::capture`'s identical contract. Zero (not an error)
    /// whenever no capture endpoint was ever discovered for this device,
    /// matching `capture`'s own documented "zero simply means nothing
    /// captured yet" convention across every backend.
    pub fn capture(&self, out: &mut [u8]) -> usize {
        unsafe { USB_AUDIO_CAPTURE_RING.pop_upto(out.as_mut_ptr(), out.len()) }
    }

    pub fn capture_overrun_count(&self) -> u32 {
        self.capture_stream().map(|stream| stream.overrun_count()).unwrap_or(0)
    }

    pub fn capture_backlog(&self) -> usize {
        USB_AUDIO_CAPTURE_RING.available()
    }

    pub fn capture_periods(&self) -> u32 {
        self.capture_stream().map(|stream| stream.entries_written()).unwrap_or(0)
    }
}

static USB_AUDIO_RING: audio::PcmRing = audio::PcmRing::new();
static USB_AUDIO_CAPTURE_RING: audio::PcmRing = audio::PcmRing::new();
/// A single-element array, not a bare `Option`, purely so every access
/// goes through indexing -- mirroring `CONTROLLERS`'s own established
/// pattern in this file -- rather than taking a shared reference to a
/// scalar `static mut` directly, which the compiler now rejects outright
/// (`static_mut_refs`) even under `unsafe`.
static mut USB_AUDIO_DEVICE: [Option<UsbAudioDevice>; 1] = [None];
static mut USB_AUDIO_DISCOVERED: bool = false;

/// Discovers a USB Audio Class device (once -- see below) and begins
/// continuously streaming `USB_AUDIO_RING` to its playback endpoint (a
/// device with no playback endpoint at all is not usable here, matching
/// every other USB Audio entry point in this file) and, when the same
/// device also offers a capture endpoint, continuously draining its
/// received packets into `USB_AUDIO_CAPTURE_RING` too -- both directions
/// serviced every `poll_background_work` call via `service_active_audio`.
/// Exactly like `audio.rs`'s own "discover once at first use, never
/// rescanned" model for ES1370/HDA: a device plugged in *after* the first
/// call simply isn't found until the next boot -- USB hot-plug support is
/// not something this facade's "discover once" design attempts anywhere,
/// for any backend.
pub fn discover_usb_audio_device() -> Option<&'static UsbAudioDevice> {
    unsafe {
        if USB_AUDIO_DISCOVERED {
            return USB_AUDIO_DEVICE[0].as_ref();
        }
        USB_AUDIO_DISCOVERED = true;
    }

    let (controller, device) = find_uac_device()?;
    let endpoint = device.playback?;
    const DEVICE_ADDRESS: u8 = 1; // matches find_uac_device's own fixed address
    let endpoint_number = endpoint.endpoint_address & 0x0f;
    let stream =
        controller.start_isoc_ring_out(DEVICE_ADDRESS, endpoint_number, endpoint.packet_bytes, &USB_AUDIO_RING)?;

    // A capture stream is genuinely optional: a playback-only device (or
    // one where the capture pool allocation happens to fail) simply
    // leaves `capture_stream`/`capture_endpoint` at `None`, and every
    // `UsbAudioDevice` capture method above already treats that as "zero
    // bytes captured", not a fatal error for the whole device.
    let capture_endpoint = device.capture;
    let capture_stream = capture_endpoint.and_then(|capture| {
        let capture_endpoint_number = capture.endpoint_address & 0x0f;
        controller.start_isoc_ring_in(DEVICE_ADDRESS, capture_endpoint_number, capture.packet_bytes, &USB_AUDIO_CAPTURE_RING)
    });

    unsafe {
        USB_AUDIO_DEVICE[0] = Some(UsbAudioDevice {
            controller,
            endpoint,
            ring: &USB_AUDIO_RING,
            stream: UnsafeCell::new(stream),
            capture_endpoint,
            capture_stream: UnsafeCell::new(capture_stream),
        });
        USB_AUDIO_DEVICE[0].as_ref()
    }
}

/// Keeps the active USB Audio device's playback stream, and its capture
/// stream if one was discovered, both primed -- called unconditionally
/// from `poll_background_work` (`shell.rs`), the same "cheap to call from
/// every long-running foreground loop" slot `nic.watchdog()` already
/// occupies there. There is no interrupt or other background path
/// driving either stream on its own: sitting idle at the local prompt
/// (where `poll_background_work` is never reached) starves them, unlike
/// ES1370/HDA's ISR-driven playback/capture, which keeps running
/// regardless -- the honest tradeoff of having no isochronous IRQ path
/// at all.
pub fn service_active_audio() {
    unsafe {
        if let Some(device) = USB_AUDIO_DEVICE[0].as_ref() {
            device.stream().service(device.controller);
            if let Some(capture) = device.capture_stream() {
                capture.service(device.controller);
            }
        }
    }
}
