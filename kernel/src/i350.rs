//! Scaffolding for Intel I350 ("Powerville") PCIe Gigabit controllers, such
//! as the dual-port card carrying an `NHI350AM4` chip marking.
//!
//! **Status: device ID confirmed, register-level behavior not yet verified
//! against real hardware.** [`CANDIDATE_DEVICE_IDS`] now carries `0x1521`
//! (I350 copper), read via `lspci -vv` from a Linux host against a physical
//! Intel Ethernet Server Adapter I350-T2 (dual-port copper, PCI address
//! `04:00.1`, currently bound to Linux's own `igb` driver) -- confirmed by
//! the reported name ("I350 Gigabit Network Connection") and subsystem
//! string, not guessed. This module is already fully wired into `nic.rs`
//! exactly the way `e1000.rs` is (this module's `Nic`-facing methods
//! deliberately match `E1000`'s method names/signatures one-for-one, which
//! made that wiring a mechanical copy-paste when it was first scaffolded),
//! so `initialize_all`/`get` will now actually find and use this card the
//! next time this kernel boots on that physical machine. What is **not**
//! yet confirmed is whether any of the register-level assumptions below
//! (offsets, the SW/FW semaphore handshake, ring enable bits) actually hold
//! against this specific card -- QEMU has no I350 model at all, so, like
//! the 3C905 and NE2000 backends, every remaining bit of verification here
//! needs the real machine, permanently, and none of it can happen from
//! this environment.
//!
//! I350 is a different, newer chip generation from the `82540EM`
//! `e1000.rs` targets (which predates PCIe entirely), but shares enough of
//! Intel's common e1000 register lineage that this module's shape mirrors
//! `e1000.rs` closely: same legacy 16-byte descriptor layout, same
//! reset/configure/send/receive/watchdog structure. Three real differences
//! are already reflected below, each cross-checked against the publicly
//! documented 82576/82580/I350 register map (Linux's `igb` driver targets
//! the same family and uses the identical offsets), but **unverified
//! against this specific physical card** -- expect to revisit every offset
//! and bit here once real hardware is available to test against, the same
//! way this project's own RTL8139/e1000/3C905 drivers all needed real-
//! hardware fixups beyond what QEMU could ever catch (see AGENTS.md's "Real
//! hardware notes"):
//!
//! 1. **Interrupt registers moved.** `82540EM` has `ICR`/`IMS`/`IMC` at
//!    `0x00c0`/`0x00d0`/`0x00d8`; I350 moved them to `0x1500`/`0x1508`/
//!    `0x150c` to make room for its multi-queue/MSI-X interrupt
//!    infrastructure, even though this driver only ever uses queue 0's
//!    legacy interrupt cause.
//! 2. **Per-queue ring registers, at different offsets, and requiring an
//!    explicit enable bit.** I350's receive/transmit rings are addressed as
//!    an array indexed by queue number (`RDBAL(n)`/`TDBAL(n)`/...) rather
//!    than 82540EM's one fixed pair of ring register sets; queue 0's own
//!    registers happen to live at `0xc000`/`0xe000`, not 82540EM's
//!    `0x2800`/`0x3800`. Each queue must also be explicitly enabled via a
//!    `RXDCTL`/`TXDCTL` "queue enable" bit (`82540EM` had no such bit --
//!    its one ring was implicitly always on) -- omitting this is a
//!    documented way to have every other register look correctly
//!    programmed while the card still never touches the ring.
//! 3. **Software/firmware register arbitration.** I350 boards commonly have
//!    a resident manageability controller (AMT/ME) that can independently
//!    touch the PHY and NVM. `acquire_swfw_sync`/`release_swfw_sync` below
//!    implement the documented two-step handshake (a simple `SWSM` hardware
//!    semaphore gating access to the richer `SW_FW_SYNC` resource-bitmap
//!    register) before this driver touches either -- `82540EM` has no such
//!    firmware to contend with, so `e1000.rs` needed nothing like this.
//!    This is the single riskiest piece of this scaffold to have gotten
//!    subtly wrong, since a stuck semaphore looks identical to a hung
//!    device from software's point of view.
//!
//! Deliberately not attempted here, consistent with this project's existing
//! scope for every other NIC backend: MSI/MSI-X (this driver only ever
//! programs queue 0's legacy interrupt cause, matching this project's
//! polling-first design -- see AGENTS.md), multiple receive/transmit
//! queues, jumbo frames, checksum/TSO offload, and SR-IOV. Notably, the
//! I350's "Virtual Function" device ID (`0x1520`) must never be added to
//! `CANDIDATE_DEVICE_IDS`: a VF exposes a much smaller, mailbox-based
//! register set with no direct PHY/link access at all, and this driver's
//! physical-function init sequence would not merely fail against one but
//! could hang indefinitely waiting on state a VF can never report.

use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use crate::interrupts;
use crate::nic::{Health, WatchdogResult};
use crate::paging;
use crate::pci;
use crate::pmem;

const VENDOR_INTEL: u16 = 0x8086;

/// Real I350 PCI device IDs (physical functions only). `0x1521` (copper) is
/// confirmed against a physical Intel Ethernet Server Adapter I350-T2 (see
/// this module's own doc comment) and is the only one enabled so far;
/// `0x1522` (fiber), `0x1523` (backplane), `0x1524` (copper, dual-port
/// variant), and `0x1526` (copper, LOM variant) remain deliberately
/// excluded until each is confirmed the same way -- fiber/backplane
/// variants in particular skip copper-PHY autonegotiation entirely, so
/// enabling them without physical testing would be guessing, not
/// confirming. **Never add `0x1520`** (I350 Virtual Function): see the
/// module doc comment.
const CANDIDATE_DEVICE_IDS: &[u16] = &[0x1521];

// --- Registers shared with e1000.rs's 82540EM layout (unchanged offsets) ---
const REG_CTRL: u32 = 0x0000;
const REG_RCTL: u32 = 0x0100;
const REG_TCTL: u32 = 0x0400;
const REG_TIPG: u32 = 0x0410;
const REG_RAL0: u32 = 0x5400;
const REG_RAH0: u32 = 0x5404;

// --- Registers that moved for I350's multi-queue/MSI-X architecture ---
const REG_ICR: u32 = 0x1500;
const REG_IMS: u32 = 0x1508;
const REG_IMC: u32 = 0x150c;

// --- Queue-0 ring registers (I350 addresses these as a per-queue array;
// only queue 0 is ever used here) ---
const REG_RDBAL0: u32 = 0xc000;
const REG_RDBAH0: u32 = 0xc004;
const REG_RDLEN0: u32 = 0xc008;
const REG_SRRCTL0: u32 = 0xc00c;
const REG_RDH0: u32 = 0xc010;
const REG_RDT0: u32 = 0xc018;
const REG_RXDCTL0: u32 = 0xc028;
const REG_TDBAL0: u32 = 0xe000;
const REG_TDBAH0: u32 = 0xe004;
const REG_TDLEN0: u32 = 0xe008;
const REG_TDH0: u32 = 0xe010;
const REG_TDT0: u32 = 0xe018;
const REG_TXDCTL0: u32 = 0xe028;

// --- Software/firmware arbitration (see module doc comment, point 3) ---
const REG_SWSM: u32 = 0x5b50;
const REG_SW_FW_SYNC: u32 = 0x5b5c;
/// `SWSM` bit 0: hardware semaphore bit indicating software may proceed to
/// claim `SW_FW_SYNC`. This is a simple test-and-set bit implemented in
/// hardware, not software-defined, so it needs no separate "clear" -- reading
/// it as already 0 (after this driver's own prior set) is how release works.
const SWSM_SMBI: u32 = 1 << 0;
/// `SWSM` bit 1: this driver's own claim on the semaphore above, distinct
/// from `SMBI`. Both must be managed together per the documented handshake.
const SWSM_SWESMBI: u32 = 1 << 1;
/// `SW_FW_SYNC` bit 0: the specific shared resource this driver ever needs
/// (PHY access for the reset/link-status sequence). Other bits in this
/// register arbitrate NVM and other shared resources this driver never
/// touches.
const SWFW_PHY0_SM: u32 = 1 << 0;

const RING_COUNT: usize = 8;
const RX_BUFFER_SIZE: usize = 2048;
const TX_BUFFER_SIZE: usize = 2048;
const MMIO_BYTES: u32 = 0x40000;

const CTRL_SLU: u32 = 1 << 6;
const CTRL_FD: u32 = 1 << 0;
const CTRL_RST: u32 = 1 << 26;

const RCTL_EN: u32 = 1 << 1;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SECRC: u32 = 1 << 26;

const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0x10 << 4;
const TCTL_COLD: u32 = 0x40 << 12;

/// Descriptor-ring "queue enable" bit, shared by `RXDCTL0`/`TXDCTL0`. Unlike
/// 82540EM's implicitly-always-on single ring, I350 requires this bit to be
/// set (and, for Rx, observed to have latched) before the ring is live --
/// see the module doc comment, point 2.
const DCTL_QUEUE_ENABLE: u32 = 1 << 25;

const RX_STATUS_DD: u8 = 1 << 0;
const RX_STATUS_EOP: u8 = 1 << 1;
const RX_ERROR_MASK: u8 = 0xff;

const TX_STATUS_DD: u8 = 1 << 0;
const TX_CMD_EOP: u8 = 1 << 0;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;

const ICR_TXDW: u32 = 1 << 0;
const ICR_LSC: u32 = 1 << 2;
const ICR_RXSEQ: u32 = 1 << 3;
const ICR_RXDMT0: u32 = 1 << 4;
const ICR_RXO: u32 = 1 << 6;
const ICR_RXT0: u32 = 1 << 7;
const INTERRUPT_MASK: u32 = ICR_TXDW | ICR_LSC | ICR_RXSEQ | ICR_RXDMT0 | ICR_RXO | ICR_RXT0;

pub const MIN_FRAME_LEN: usize = 60;
pub const MAX_FRAME_LEN: usize = 1514;

#[repr(C)]
#[derive(Clone, Copy)]
struct RxDescriptor {
    buffer_address: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TxDescriptor {
    buffer_address: u64,
    length: u16,
    checksum_offset: u8,
    command: u8,
    status: u8,
    checksum_start: u8,
    special: u16,
}

fn register_pointer(base: u32, offset: u32) -> *mut u32 {
    (base as usize + offset as usize) as *mut u32
}

fn read_register(base: u32, offset: u32) -> u32 {
    unsafe { read_volatile(register_pointer(base, offset)) }
}

fn write_register(base: u32, offset: u32, value: u32) {
    unsafe { write_volatile(register_pointer(base, offset), value) }
}

fn descriptor_pointer<T>(base: u32, index: usize) -> *mut T {
    (base as usize + index * core::mem::size_of::<T>()) as *mut T
}

/// Acquires the `SWSM`/`SW_FW_SYNC` handshake for `SWFW_PHY0_SM` before
/// touching the PHY, per the documented Intel 82576/I350-family protocol
/// (the same one Linux's `igb` driver implements as
/// `e1000_acquire_swfw_sync_i210`). **Unverified against real hardware**:
/// this is a best-effort transcription meant to be confirmed (or corrected)
/// the first time this module is actually tested against the physical
/// card, not a claim that it is already known-correct.
fn acquire_swfw_sync(mmio_base: u32) -> Result<(), InitError> {
    // Step 1: win the simple hardware SWSM semaphore, bounded so a firmware
    // fault (or a wrong offset above) reports an error instead of hanging
    // the boot forever.
    let mut attempts = 100_000u32;
    loop {
        if read_register(mmio_base, REG_SWSM) & SWSM_SMBI == 0 {
            break;
        }
        attempts -= 1;
        if attempts == 0 {
            return Err(InitError::FirmwareArbitrationTimedOut);
        }
        core::hint::spin_loop();
    }
    let swsm = read_register(mmio_base, REG_SWSM);
    write_register(mmio_base, REG_SWSM, swsm | SWSM_SWESMBI);

    // Step 2: claim the specific resource bit in SW_FW_SYNC, still under
    // SWSM's protection.
    let mut attempts = 100_000u32;
    loop {
        let sync = read_register(mmio_base, REG_SW_FW_SYNC);
        if sync & SWFW_PHY0_SM == 0 {
            write_register(mmio_base, REG_SW_FW_SYNC, sync | SWFW_PHY0_SM);
            break;
        }
        attempts -= 1;
        if attempts == 0 {
            // Release SWSM before giving up, so a failed acquire never
            // itself leaves the semaphore stuck for the next attempt.
            let swsm = read_register(mmio_base, REG_SWSM);
            write_register(mmio_base, REG_SWSM, swsm & !(SWSM_SMBI | SWSM_SWESMBI));
            return Err(InitError::FirmwareArbitrationTimedOut);
        }
        core::hint::spin_loop();
    }

    let swsm = read_register(mmio_base, REG_SWSM);
    write_register(mmio_base, REG_SWSM, swsm & !(SWSM_SMBI | SWSM_SWESMBI));
    Ok(())
}

/// Releases the `SWFW_PHY0_SM` claim taken by `acquire_swfw_sync`, so
/// resident management firmware (if any) can use the PHY again.
fn release_swfw_sync(mmio_base: u32) {
    let sync = read_register(mmio_base, REG_SW_FW_SYNC);
    write_register(mmio_base, REG_SW_FW_SYNC, sync & !SWFW_PHY0_SM);
}

/// Reads `RAL0`/`RAH0`, retrying with a short delay if the result is not
/// yet a plausible MAC (all-zero or all-`0xff`), instead of failing on
/// the very first read.
///
/// **Confirmed real-hardware finding**, on the same Dell Precision
/// 3620's onboard I350: `reset` (above) only ever polls `CTRL.RST`
/// clearing before returning, the same completion signal that is
/// correct for the older 82540EM/e1000 family -- but `e1000e.rs`'s own
/// module doc comment already documents a hazard where a *newer* Intel
/// NIC generation loads its receive-address registers from NVM
/// asynchronously, well after `CTRL.RST` itself has cleared, and reading
/// them too early (as `configure` used to do here, immediately and only
/// once) returns garbage -- for e1000e that garbage happened to be all
/// zero/`0xff` too, exactly what was observed here: both real I350 ports
/// on this same machine came back `InvalidMac` every time, never merely
/// intermittently, consistent with reading the registers too early
/// rather than a genuinely absent/corrupt NVM-programmed address.
/// e1000e's own fix polls `EECD.AUTO_RD`, a documented, named completion
/// bit for that family; this project has no confirmed equivalent bit
/// offset for I350 to poll the same way (no datasheet at hand while
/// fixing this against the real machine that found it), so this retries
/// the read itself instead, on the reasoning that whatever completion
/// signal exists, it must complete within a bounded, short real time
/// after reset -- the same "retry the actual observable outcome instead
/// of a specific status bit" idiom already accepted elsewhere in this
/// project when a real register meaning is not yet known. If this
/// still returns `None` on some other real I350 board, the next
/// diagnostic step is finding and polling the real completion bit
/// directly, not lengthening this retry further.
///
/// **Confirmed fixed end to end**: both real I350 ports on this machine
/// now enumerate with valid, sequential MAC addresses and pass real
/// traffic (`nic send`/`term net` both confirmed over either port).
fn read_mac_with_retry(mmio_base: u32) -> Option<[u8; 6]> {
    let mut attempts = 20u32;
    loop {
        let ral = read_register(mmio_base, REG_RAL0);
        let rah = read_register(mmio_base, REG_RAH0);
        let mut mac = [0u8; 6];
        mac[0..4].copy_from_slice(&ral.to_le_bytes());
        mac[4..6].copy_from_slice(&(rah as u16).to_le_bytes());
        if mac.iter().any(|&byte| byte != 0) && mac != [0xff; 6] {
            return Some(mac);
        }
        attempts -= 1;
        if attempts == 0 {
            return None;
        }
        let deadline = interrupts::ticks().wrapping_add(1);
        while interrupts::ticks() < deadline {
            core::hint::spin_loop();
        }
    }
}

pub struct I350 {
    mmio_base: u32,
    mac: Cell<[u8; 6]>,
    rx_descriptors: u32,
    tx_descriptors: u32,
    rx_buffers: [u32; RING_COUNT],
    tx_buffers: [u32; RING_COUNT],
    rx_next: Cell<usize>,
    tx_next: Cell<usize>,
    overflow_count: Cell<u32>,
    rx_progress: Cell<u32>,
    tx_progress: Cell<u32>,
    recovery_count: Cell<u32>,
    recovery_failures: Cell<u32>,
    recovery_pending: AtomicBool,
    promiscuous: Cell<bool>,
    irq: Option<u8>,
    interrupt_count: AtomicU32,
    interrupt_errors: AtomicU32,
    work_pending: AtomicBool,
}

#[derive(Debug)]
pub enum InitError {
    OutOfMemory,
    MappingUnavailable,
    ResetTimedOut,
    InvalidMac,
    FirmwareArbitrationTimedOut,
    QueueEnableTimedOut,
}

impl I350 {
    pub fn probe_all(out: &mut [ProbeInfo]) -> usize {
        let mut addresses = [None; pci::MAX_DEVICE_MATCHES];
        let found = pci::find_devices(VENDOR_INTEL, CANDIDATE_DEVICE_IDS, &mut addresses);
        let mut count = 0;
        for address in addresses[..found.min(addresses.len())].iter().flatten() {
            address.enable_memory_and_bus_master();
            if let Some(mmio_base) = address.memory_bar(0) {
                if count == out.len() {
                    break;
                }
                out[count] = ProbeInfo {
                    mmio_base,
                    irq: address.interrupt_line(),
                };
                count += 1;
            }
        }
        count
    }

    pub fn init(mmio_base: u32, irq: Option<u8>) -> Result<Self, InitError> {
        paging::map_mmio(mmio_base, MMIO_BYTES).map_err(|_| InitError::MappingUnavailable)?;
        Self::reset(mmio_base)?;

        let rx_descriptors = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let tx_descriptors = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        let mut rx_buffers = [0u32; RING_COUNT];
        for buffer in &mut rx_buffers {
            *buffer = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        }
        let mut tx_buffers = [0u32; RING_COUNT];
        for buffer in &mut tx_buffers {
            *buffer = pmem::allocate_contiguous(1).ok_or(InitError::OutOfMemory)?;
        }

        let nic = Self {
            mmio_base,
            mac: Cell::new([0; 6]),
            rx_descriptors,
            tx_descriptors,
            rx_buffers,
            tx_buffers,
            rx_next: Cell::new(0),
            tx_next: Cell::new(0),
            overflow_count: Cell::new(0),
            rx_progress: Cell::new(0),
            tx_progress: Cell::new(0),
            recovery_count: Cell::new(0),
            recovery_failures: Cell::new(0),
            recovery_pending: AtomicBool::new(false),
            promiscuous: Cell::new(false),
            irq,
            interrupt_count: AtomicU32::new(0),
            interrupt_errors: AtomicU32::new(0),
            work_pending: AtomicBool::new(false),
        };
        nic.configure()?;
        Ok(nic)
    }

    fn reset(mmio_base: u32) -> Result<(), InitError> {
        acquire_swfw_sync(mmio_base)?;
        write_register(mmio_base, REG_CTRL, CTRL_RST);
        let mut result = Err(InitError::ResetTimedOut);
        for _ in 0..100_000 {
            if read_register(mmio_base, REG_CTRL) & CTRL_RST == 0 {
                result = Ok(());
                break;
            }
            core::hint::spin_loop();
        }
        release_swfw_sync(mmio_base);
        result
    }

    fn configure(&self) -> Result<(), InitError> {
        let mac = if self.mac.get() == [0; 6] {
            let mac = read_mac_with_retry(self.mmio_base).ok_or(InitError::InvalidMac)?;
            self.mac.set(mac);
            mac
        } else {
            self.mac.get()
        };
        write_register(
            self.mmio_base,
            REG_RAL0,
            u32::from_le_bytes([mac[0], mac[1], mac[2], mac[3]]),
        );
        write_register(
            self.mmio_base,
            REG_RAH0,
            u32::from(u16::from_le_bytes([mac[4], mac[5]])) | (1 << 31),
        );

        write_register(self.mmio_base, REG_IMC, u32::MAX);
        let _ = read_register(self.mmio_base, REG_ICR);

        for index in 0..RING_COUNT {
            let descriptor = descriptor_pointer::<RxDescriptor>(self.rx_descriptors, index);
            unsafe {
                write_volatile(
                    descriptor,
                    RxDescriptor {
                        buffer_address: u64::from(self.rx_buffers[index]),
                        length: 0,
                        checksum: 0,
                        status: 0,
                        errors: 0,
                        special: 0,
                    },
                );
            }
        }
        for index in 0..RING_COUNT {
            let descriptor = descriptor_pointer::<TxDescriptor>(self.tx_descriptors, index);
            unsafe {
                write_volatile(
                    descriptor,
                    TxDescriptor {
                        buffer_address: u64::from(self.tx_buffers[index]),
                        length: 0,
                        checksum_offset: 0,
                        command: 0,
                        status: TX_STATUS_DD,
                        checksum_start: 0,
                        special: 0,
                    },
                );
            }
        }

        write_register(self.mmio_base, REG_RDBAL0, self.rx_descriptors);
        write_register(self.mmio_base, REG_RDBAH0, 0);
        write_register(
            self.mmio_base,
            REG_RDLEN0,
            (RING_COUNT * core::mem::size_of::<RxDescriptor>()) as u32,
        );
        // SRRCTL's low bits give the receive buffer size in 1 KiB units for
        // legacy (non-"advanced") descriptors; RX_BUFFER_SIZE is 2048.
        write_register(self.mmio_base, REG_SRRCTL0, (RX_BUFFER_SIZE / 1024) as u32);
        write_register(self.mmio_base, REG_RDH0, 0);
        write_register(self.mmio_base, REG_RDT0, 0);

        write_register(self.mmio_base, REG_TDBAL0, self.tx_descriptors);
        write_register(self.mmio_base, REG_TDBAH0, 0);
        write_register(
            self.mmio_base,
            REG_TDLEN0,
            (RING_COUNT * core::mem::size_of::<TxDescriptor>()) as u32,
        );
        write_register(self.mmio_base, REG_TDH0, 0);
        write_register(self.mmio_base, REG_TDT0, 0);

        write_register(self.mmio_base, REG_TIPG, 0x0060_200a);
        write_register(
            self.mmio_base,
            REG_TCTL,
            TCTL_EN | TCTL_PSP | TCTL_CT | TCTL_COLD,
        );
        let promiscuous = if self.promiscuous.get() {
            RCTL_UPE | RCTL_MPE
        } else {
            0
        };
        write_register(
            self.mmio_base,
            REG_RCTL,
            RCTL_EN | RCTL_BAM | RCTL_SECRC | promiscuous,
        );
        write_register(self.mmio_base, REG_CTRL, CTRL_SLU | CTRL_FD);

        // Unlike 82540EM's one implicitly-on ring, I350 requires each queue
        // to be explicitly enabled and, for Rx, that enable observed to
        // latch before the ring is trusted to be live -- see the module
        // doc comment, point 2. RDT is only advanced to "make buffers
        // available" after the queue reports itself enabled, matching the
        // documented bring-up order.
        write_register(self.mmio_base, REG_TXDCTL0, DCTL_QUEUE_ENABLE);
        write_register(self.mmio_base, REG_RXDCTL0, DCTL_QUEUE_ENABLE);
        let mut attempts = 100_000u32;
        loop {
            if read_register(self.mmio_base, REG_RXDCTL0) & DCTL_QUEUE_ENABLE != 0 {
                break;
            }
            attempts -= 1;
            if attempts == 0 {
                return Err(InitError::QueueEnableTimedOut);
            }
            core::hint::spin_loop();
        }
        write_register(self.mmio_base, REG_RDT0, (RING_COUNT - 1) as u32);

        Ok(())
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac.get()
    }

    // link_status() was deliberately removed here after a real-hardware
    // finding: unlike e1000e.rs (confirmed correct and safe at the same
    // offset), reading REG_STATUS on this real I350 while it was the
    // active interface consistently made the `nic`/`nic list` shell
    // commands never reply at all, while every other command on the
    // same connection (including `nic use N` itself) kept working fine
    // and the machine's own uptime kept advancing normally (ruling out
    // a crash/reset). I350's own register map moved substantially from
    // the classic 82540EM layout for its multi-queue architecture (see
    // this file's other REG_* comments); assuming STATUS stayed at the
    // classic 0x0008 offset without confirming it against a real
    // datasheet was the likely mistake. Do not re-add this without
    // confirming the real offset first (e.g. via a Linux `ethtool -d`
    // register dump on real I350 hardware, the same technique that found
    // e1000e's TCTL fix elsewhere in this project).

    pub fn set_promiscuous(&self, enabled: bool) {
        let mut value = read_register(self.mmio_base, REG_RCTL);
        if enabled {
            value |= RCTL_UPE | RCTL_MPE;
        } else {
            value &= !(RCTL_UPE | RCTL_MPE);
        }
        write_register(self.mmio_base, REG_RCTL, value);
        self.promiscuous.set(enabled);
    }

    pub fn overflow_count(&self) -> u32 {
        self.overflow_count.get()
    }

    pub fn health(&self) -> Health {
        Health {
            rx_progress: self.rx_progress.get(),
            tx_progress: self.tx_progress.get(),
            recoveries: self.recovery_count.get(),
            recovery_failures: self.recovery_failures.get(),
            recovery_pending: self.recovery_pending.load(Ordering::Acquire),
        }
    }

    /// RX overrun and a bounded transmit-completion timeout are documented
    /// loss/stall conditions. Rebuild the existing descriptor rings in
    /// place; no DMA frame is allocated or freed during recovery.
    pub fn watchdog(&self) -> WatchdogResult {
        if !self.recovery_pending.swap(false, Ordering::AcqRel) {
            return WatchdogResult::Healthy;
        }

        if Self::reset(self.mmio_base).is_err() || self.configure().is_err() {
            self.recovery_failures
                .set(self.recovery_failures.get().wrapping_add(1));
            return WatchdogResult::Failed;
        }
        self.rx_next.set(0);
        self.tx_next.set(0);
        self.enable_interrupts();
        self.recovery_count
            .set(self.recovery_count.get().wrapping_add(1));
        WatchdogResult::Recovered
    }

    pub fn enable_interrupts(&self) {
        let _ = read_register(self.mmio_base, REG_ICR);
        write_register(self.mmio_base, REG_IMS, INTERRUPT_MASK);
    }

    pub fn handle_interrupt(&self) {
        let cause = read_register(self.mmio_base, REG_ICR);
        if cause == 0 {
            return;
        }
        self.interrupt_count.fetch_add(1, Ordering::Relaxed);
        if cause & (ICR_RXO | ICR_RXSEQ) != 0 {
            self.interrupt_errors.fetch_add(1, Ordering::Relaxed);
        }
        if cause & ICR_RXO != 0 {
            self.recovery_pending.store(true, Ordering::Release);
        }
        self.work_pending.store(true, Ordering::Release);
    }

    pub fn interrupt_stats(&self) -> (Option<u8>, u32, u32, bool) {
        (
            self.irq,
            self.interrupt_count.load(Ordering::Relaxed),
            self.interrupt_errors.load(Ordering::Relaxed),
            self.work_pending.load(Ordering::Acquire),
        )
    }

    pub fn send(&self, frame: &[u8]) -> bool {
        if frame.len() > MAX_FRAME_LEN {
            return false;
        }
        let index = self.tx_next.get();
        let descriptor = descriptor_pointer::<TxDescriptor>(self.tx_descriptors, index);
        let mut attempts = 1_000_000u32;
        while unsafe { read_volatile(&(*descriptor).status) } & TX_STATUS_DD == 0 {
            attempts -= 1;
            if attempts == 0 {
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
            core::hint::spin_loop();
        }

        let padded_len = frame.len().max(MIN_FRAME_LEN);
        unsafe {
            let buffer = core::slice::from_raw_parts_mut(
                self.tx_buffers[index] as *mut u8,
                TX_BUFFER_SIZE,
            );
            buffer[..frame.len()].copy_from_slice(frame);
            buffer[frame.len()..padded_len].fill(0);
            write_volatile(
                descriptor,
                TxDescriptor {
                    buffer_address: u64::from(self.tx_buffers[index]),
                    length: padded_len as u16,
                    checksum_offset: 0,
                    command: TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS,
                    status: 0,
                    checksum_start: 0,
                    special: 0,
                },
            );
        }
        fence(Ordering::SeqCst);
        write_register(self.mmio_base, REG_TDT0, ((index + 1) % RING_COUNT) as u32);

        let mut attempts = 1_000_000u32;
        while unsafe { read_volatile(&(*descriptor).status) } & TX_STATUS_DD == 0 {
            attempts -= 1;
            if attempts == 0 {
                self.recovery_pending.store(true, Ordering::Release);
                return false;
            }
            core::hint::spin_loop();
        }
        self.tx_next.set((index + 1) % RING_COUNT);
        self.tx_progress.set(self.tx_progress.get().wrapping_add(1));
        true
    }

    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        self.work_pending.store(false, Ordering::Release);
        let index = self.rx_next.get();
        let descriptor = descriptor_pointer::<RxDescriptor>(self.rx_descriptors, index);
        let status = unsafe { read_volatile(&(*descriptor).status) };
        if status & RX_STATUS_DD == 0 {
            return None;
        }

        let length = unsafe { read_volatile(&(*descriptor).length) } as usize;
        let errors = unsafe { read_volatile(&(*descriptor).errors) };
        let complete = status & RX_STATUS_EOP != 0;
        let valid =
            complete && errors & RX_ERROR_MASK == 0 && length != 0 && length <= RX_BUFFER_SIZE;
        let copy_len = length.min(out.len()).min(MAX_FRAME_LEN);
        if valid {
            unsafe {
                let source =
                    core::slice::from_raw_parts(self.rx_buffers[index] as *const u8, copy_len);
                out[..copy_len].copy_from_slice(source);
            }
        }
        unsafe {
            write_volatile(&mut (*descriptor).status, 0);
            write_volatile(&mut (*descriptor).errors, 0);
        }
        write_register(self.mmio_base, REG_RDT0, index as u32);
        self.rx_next.set((index + 1) % RING_COUNT);

        if !valid {
            self.overflow_count.set(self.overflow_count.get() + 1);
            return None;
        }
        self.rx_progress.set(self.rx_progress.get().wrapping_add(1));
        Some(copy_len)
    }
}

#[derive(Clone, Copy)]
pub struct ProbeInfo {
    mmio_base: u32,
    irq: Option<u8>,
}

const MAX_INSTANCES: usize = pci::MAX_DEVICE_MATCHES;
static mut NICS: [Option<I350>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
static mut NIC_COUNT: usize = 0;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed,
}

pub fn initialize_all() -> Result<usize, Status> {
    unsafe {
        if !INITIALIZED {
            INITIALIZED = true;
            let mut probe_info = [const {
                ProbeInfo {
                    mmio_base: 0,
                    irq: None,
                }
            }; MAX_INSTANCES];
            let found = I350::probe_all(&mut probe_info);
            let mut first_error = None;
            for info in &probe_info[..found] {
                match I350::init(info.mmio_base, info.irq) {
                    Ok(initialized) => {
                        NICS[NIC_COUNT] = Some(initialized);
                        NIC_COUNT += 1;
                    }
                    Err(error) if first_error.is_none() => first_error = Some(error),
                    Err(_) => {}
                }
            }
            if NIC_COUNT == 0 {
                return if first_error.is_some() {
                    Err(Status::InitFailed)
                } else {
                    Err(Status::NotPresent)
                };
            }
        }
        Ok(NIC_COUNT)
    }
}

pub fn get(index: usize) -> Option<&'static I350> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
