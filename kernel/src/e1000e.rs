//! Intel 82574L/82579 ("e1000e" family) PCIe Gigabit Ethernet driver.
//!
//! Two candidate device IDs, for two different reasons -- the same "one
//! confirmed real device, one QEMU model" shape `ac97.rs` uses, not a
//! speculative broadening across the wider e1000e device-ID family:
//!
//! - `0x1503` (82579V): confirmed real hardware, read via `lspci -vv`
//!   against a physical ITX motherboard's onboard NIC. This is the actual
//!   target this driver was built for.
//! - `0x10d3` (82574L): QEMU's own `-device e1000e` model. Not the same
//!   silicon as 82579 -- see below -- but sharing enough of the classic
//!   e1000 register lineage that it exercises the same ring/reset/send/
//!   receive plumbing this driver needs for 82579 too, giving this driver
//!   real `make test` coverage that pure real-hardware-only backends like
//!   `i350.rs`/`threecom.rs` never get.
//!
//! **82579 (client, ICH/PCH-integrated) and 82574 (PCIe add-in-card-style)
//! are handled by genuinely different code paths in Linux's own `e1000e`
//! driver** (`ich8lan.c` vs `82571.c`), not just two device IDs on one
//! chip. The part they share -- and this driver relies on -- is the
//! classic single-queue legacy descriptor/ring register layout, unchanged
//! from `e1000.rs`'s 82540EM offsets (`CTRL`/`ICR`/`IMS`/`IMC`/`RCTL`/
//! `TCTL`/`TIPG`/`RDBAL..RDT`/`TDBAL..TDT`/`RAL0`/`RAH0`), so this module's
//! shape mirrors `e1000.rs` closely -- same as `i350.rs` does for its own,
//! differently-shaped chip family.
//!
//! One real, chip-specific difference is implemented here, gated to run
//! only for the 82579 IDs (never for 82574L, which uses a different,
//! I350-style `SWSM`/`SW_FW_SYNC` arbitration protocol this driver does not
//! implement -- 82574L is QEMU-only here, so getting that mechanism
//! exactly right was not worth guessing against hardware this project has
//! no access to):
//!
//! - **Firmware/PHY register arbitration.** 82579 sits on the same silicon
//!   as Intel's Management Engine, which can independently touch the PHY.
//!   `acquire_pch_semaphore`/`release_pch_semaphore` implement the
//!   documented single-bit `EXTCNF_CTRL.MDIO_SW_OWNERSHIP` handshake (set
//!   the bit, then read back to confirm hardware actually granted it,
//!   since a losing request simply never latches) around reset -- a
//!   simpler, single-register mechanism than I350's two-step `SWSM` +
//!   `SW_FW_SYNC` protocol, and a genuinely different one, not a
//!   generalization of it. **Unverified against real hardware**, the same
//!   caveat `i350.rs`'s own semaphore implementation carries: this is a
//!   best-effort transcription to be confirmed (or corrected) the first
//!   time this module actually runs against one of the target ITX boards.
//!
//! **Deliberately not implemented: 82579's documented reset-hang erratum
//! workaround.** Intel's spec update documents a real hang risk on a plain
//! `CTRL.RST` against this exact chip, worked around in Linux's
//! `ich8lan.c` with a specific PHY-register pre-reset sequence. Guessing
//! that sequence from memory here risked getting a real-hardware-hang-
//! prone fix subtly wrong in exactly the way that's hardest to diagnose
//! remotely -- so this is left as the single most likely real-hardware
//! bring-up snag to watch for, not a pre-implemented (and unverifiable-
//! from-here) fix. If reset hangs on real 82579 hardware, this is the
//! first place to look.
//!
//! Also deliberately out of scope, consistent with every other NIC backend
//! here: MSI (82579 supports it -- confirmed present in the device's own
//! PCI capability list -- but this driver only ever uses the legacy PCI
//! IRQ path, matching `e1000.rs`/`i350.rs`), the "driver loaded" `CTRL_EXT`
//! handshake with the Management Engine, K1/EEE/ULP link-power
//! workarounds, and any flash/NVM access (the MAC address is read from
//! `RAL0`/`RAH0` after reset, already loaded by hardware from NVM, exactly
//! like `e1000.rs`/`i350.rs` -- no direct flash/SPI access is ever
//! needed), jumbo frames, and checksum/TSO offload.

use core::cell::Cell;
use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering, fence};

use crate::nic::{Health, WatchdogResult};
use crate::paging;
use crate::pci;
use crate::pmem;

const VENDOR_INTEL: u16 = 0x8086;

/// QEMU's `-device e1000e` model (82574L) -- see module doc comment.
const DEVICE_82574L: u16 = 0x10d3;
/// Confirmed real hardware (82579V) -- see module doc comment.
const DEVICE_82579V: u16 = 0x1503;
/// 82579LM (vPro/AMT-branded sibling of 82579V) remains deliberately
/// excluded until confirmed against a physical board carrying it -- the
/// same "narrow now, broaden once tested" rule `i350.rs` follows for its
/// own untested sibling IDs.
#[allow(dead_code)]
const DEVICE_82579LM: u16 = 0x1502;

const CANDIDATE_DEVICE_IDS: &[u16] = &[DEVICE_82574L, DEVICE_82579V];

// --- Registers shared with e1000.rs's 82540EM layout (unchanged offsets;
// both 82574L and 82579 keep this classic legacy single-queue layout,
// unlike I350's multi-queue register map) ---
const REG_CTRL: u32 = 0x0000;
const REG_ICR: u32 = 0x00c0;
const REG_IMS: u32 = 0x00d0;
const REG_IMC: u32 = 0x00d8;
const REG_RCTL: u32 = 0x0100;
const REG_TCTL: u32 = 0x0400;
const REG_TIPG: u32 = 0x0410;
const REG_RDBAL: u32 = 0x2800;
const REG_RDBAH: u32 = 0x2804;
const REG_RDLEN: u32 = 0x2808;
const REG_RDH: u32 = 0x2810;
const REG_RDT: u32 = 0x2818;
const REG_TDBAL: u32 = 0x3800;
const REG_TDBAH: u32 = 0x3804;
const REG_TDLEN: u32 = 0x3808;
const REG_TDH: u32 = 0x3810;
const REG_TDT: u32 = 0x3818;
const REG_RAL0: u32 = 0x5400;
const REG_RAH0: u32 = 0x5404;

// --- 82579-only PCH firmware/PHY arbitration (see module doc comment) ---
const REG_EXTCNF_CTRL: u32 = 0x0f00;
/// `EXTCNF_CTRL` bit 5: request/confirm software ownership of MDIO (PHY)
/// access. Unlike I350's `SWSM`, this single register/bit *is* the whole
/// protocol: a losing request simply never reads back set.
const EXTCNF_CTRL_MDIO_SW_OWNERSHIP: u32 = 1 << 5;

/// 82579's BAR0 is 128 KiB (confirmed via `lspci -vv`); 82574L's is the
/// same size. Larger than `e1000.rs`'s 82540EM window (24 KiB), but still
/// one fixed, conservative size covering either chip.
const MMIO_BYTES: u32 = 0x20000;

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

const RING_COUNT: usize = 8;
const RX_BUFFER_SIZE: usize = 2048;
const TX_BUFFER_SIZE: usize = 2048;

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

fn is_pch_family(device_id: u16) -> bool {
    device_id == DEVICE_82579V
}

/// Acquires `EXTCNF_CTRL.MDIO_SW_OWNERSHIP` before touching PHY-adjacent
/// state around reset, per the documented ICH8LAN-family protocol (see
/// module doc comment). Only ever called for the 82579 IDs.
/// **Unverified against real hardware.**
fn acquire_pch_semaphore(mmio_base: u32) -> Result<(), InitError> {
    let mut attempts = 100_000u32;
    loop {
        let value = read_register(mmio_base, REG_EXTCNF_CTRL);
        write_register(
            mmio_base,
            REG_EXTCNF_CTRL,
            value | EXTCNF_CTRL_MDIO_SW_OWNERSHIP,
        );
        if read_register(mmio_base, REG_EXTCNF_CTRL) & EXTCNF_CTRL_MDIO_SW_OWNERSHIP != 0 {
            return Ok(());
        }
        attempts -= 1;
        if attempts == 0 {
            return Err(InitError::FirmwareArbitrationTimedOut);
        }
        core::hint::spin_loop();
    }
}

/// Releases the ownership bit `acquire_pch_semaphore` took, so resident
/// management firmware (if any) can use the PHY again.
fn release_pch_semaphore(mmio_base: u32) {
    let value = read_register(mmio_base, REG_EXTCNF_CTRL);
    write_register(
        mmio_base,
        REG_EXTCNF_CTRL,
        value & !EXTCNF_CTRL_MDIO_SW_OWNERSHIP,
    );
}

pub struct E1000E {
    mmio_base: u32,
    /// Cached at `init` time from the matched device ID; controls whether
    /// `reset` performs the 82579-only PCH semaphore handshake. 82574L
    /// (QEMU) never sets this -- see module doc comment.
    is_pch: bool,
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
}

impl E1000E {
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
                    device_id: address.device_id(),
                };
                count += 1;
            }
        }
        count
    }

    pub fn init(mmio_base: u32, irq: Option<u8>, device_id: u16) -> Result<Self, InitError> {
        paging::map_mmio(mmio_base, MMIO_BYTES).map_err(|_| InitError::MappingUnavailable)?;
        let is_pch = is_pch_family(device_id);
        Self::reset(mmio_base, is_pch)?;

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
            is_pch,
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

    fn reset(mmio_base: u32, is_pch: bool) -> Result<(), InitError> {
        if is_pch {
            acquire_pch_semaphore(mmio_base)?;
        }
        write_register(mmio_base, REG_CTRL, CTRL_RST);
        let mut result = Err(InitError::ResetTimedOut);
        for _ in 0..100_000 {
            if read_register(mmio_base, REG_CTRL) & CTRL_RST == 0 {
                result = Ok(());
                break;
            }
            core::hint::spin_loop();
        }
        if is_pch {
            release_pch_semaphore(mmio_base);
        }
        result
    }

    fn configure(&self) -> Result<(), InitError> {
        let mac = if self.mac.get() == [0; 6] {
            let ral = read_register(self.mmio_base, REG_RAL0);
            let rah = read_register(self.mmio_base, REG_RAH0);
            let mut mac = [0u8; 6];
            mac[0..4].copy_from_slice(&ral.to_le_bytes());
            mac[4..6].copy_from_slice(&(rah as u16).to_le_bytes());
            if mac.iter().all(|&byte| byte == 0) || mac == [0xff; 6] {
                return Err(InitError::InvalidMac);
            }
            self.mac.set(mac);
            mac
        } else {
            self.mac.get()
        };
        // A reset may clear the receive-address registers even though the
        // NVM-backed address was readable during initial probe. Reinstall
        // the cached valid address before turning Rx back on.
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

        self.program_ring(
            REG_RDBAL,
            REG_RDBAH,
            REG_RDLEN,
            REG_RDH,
            REG_RDT,
            self.rx_descriptors,
            RING_COUNT,
        );
        self.program_ring(
            REG_TDBAL,
            REG_TDBAH,
            REG_TDLEN,
            REG_TDH,
            REG_TDT,
            self.tx_descriptors,
            RING_COUNT,
        );
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
        Ok(())
    }

    fn program_ring(
        &self,
        low: u32,
        high: u32,
        length: u32,
        head: u32,
        tail: u32,
        base: u32,
        count: usize,
    ) {
        write_register(self.mmio_base, low, base);
        write_register(self.mmio_base, high, 0);
        write_register(
            self.mmio_base,
            length,
            (count * core::mem::size_of::<RxDescriptor>()) as u32,
        );
        write_register(self.mmio_base, head, 0);
        write_register(self.mmio_base, tail, (count - 1) as u32);
    }

    pub fn mac_address(&self) -> [u8; 6] {
        self.mac.get()
    }

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

        if Self::reset(self.mmio_base, self.is_pch).is_err() || self.configure().is_err() {
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
        write_register(self.mmio_base, REG_TDT, ((index + 1) % RING_COUNT) as u32);

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
        write_register(self.mmio_base, REG_RDT, index as u32);
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
    device_id: u16,
}

const MAX_INSTANCES: usize = pci::MAX_DEVICE_MATCHES;
static mut NICS: [Option<E1000E>; MAX_INSTANCES] = [const { None }; MAX_INSTANCES];
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
                    device_id: 0,
                }
            }; MAX_INSTANCES];
            let found = E1000E::probe_all(&mut probe_info);
            let mut first_error = None;
            for info in &probe_info[..found] {
                match E1000E::init(info.mmio_base, info.irq, info.device_id) {
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

pub fn get(index: usize) -> Option<&'static E1000E> {
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}
