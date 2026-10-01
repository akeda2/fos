//! Common kernel-owned Ethernet facade.
//!
//! Networking tapes see one stable raw-Ethernet callback contract. This
//! facade initializes every supported PCI backend instance, keeps the first
//! initialized card active by default, and lets shell commands and newer
//! tapes select a specific interface without exposing PCI or DMA details.
//! Device IRQs only acknowledge a cause and make a foreground poll prompt;
//! all descriptor and protocol parsing remains in foreground code.

use crate::println;
use crate::{e1000, e1000e, i350, interrupts, ne2000, pci, rtl8139, rtl8168, threecom};

pub const MAX_FRAME_LEN: usize = 1514;
const MAX_NICS: usize = pci::MAX_DEVICE_MATCHES * 4;

pub enum Nic {
    Rtl8139(&'static rtl8139::Rtl8139),
    ThreeCom(&'static threecom::ThreeCom),
    E1000(&'static e1000::E1000),
    E1000E(&'static e1000e::E1000E),
    I350(&'static i350::I350),
    Ne2000(&'static ne2000::Ne2000),
    Rtl8168(&'static rtl8168::Rtl8168),
}

#[derive(Clone, Copy)]
pub struct InterruptStats {
    pub irq: Option<u8>,
    pub count: u32,
    pub errors: u32,
    pub pending: bool,
}

/// Foreground-only driver health counters. `rx_progress` and `tx_progress`
/// count completed frames, not IRQs: polling remains the authority for ring
/// ownership even when a legacy line is available.
#[derive(Clone, Copy)]
pub struct Health {
    pub rx_progress: u32,
    pub tx_progress: u32,
    pub recoveries: u32,
    pub recovery_failures: u32,
    pub recovery_pending: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum WatchdogResult {
    Healthy,
    Recovered,
    Failed,
}

impl Nic {
    pub fn driver_name(&self) -> &'static str {
        match self {
            Nic::Rtl8139(_) => "RTL8139",
            Nic::ThreeCom(_) => "3C905",
            Nic::E1000(_) => "Intel e1000",
            Nic::E1000E(_) => "Intel e1000e",
            Nic::I350(_) => "Intel I350",
            Nic::Ne2000(_) => "NE2000-compatible",
            Nic::Rtl8168(_) => "Realtek RTL8168",
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            Nic::Rtl8139(nic) => nic.mac_address(),
            Nic::ThreeCom(nic) => nic.mac_address(),
            Nic::E1000(nic) => nic.mac_address(),
            Nic::E1000E(nic) => nic.mac_address(),
            Nic::I350(nic) => nic.mac_address(),
            Nic::Ne2000(nic) => nic.mac_address(),
            Nic::Rtl8168(nic) => nic.mac_address(),
        }
    }

    pub fn link_up(&self) -> Option<bool> {
        match self {
            Nic::Rtl8139(_)
            | Nic::E1000(_)
            | Nic::E1000E(_)
            | Nic::I350(_)
            | Nic::Ne2000(_)
            | Nic::Rtl8168(_) => None,
            Nic::ThreeCom(nic) => Some(nic.link_up()),
        }
    }

    pub fn send(&self, frame: &[u8]) -> bool {
        match self {
            Nic::Rtl8139(nic) => nic.send(frame),
            Nic::ThreeCom(nic) => nic.send(frame),
            Nic::E1000(nic) => nic.send(frame),
            Nic::E1000E(nic) => nic.send(frame),
            Nic::I350(nic) => nic.send(frame),
            Nic::Ne2000(nic) => nic.send(frame),
            Nic::Rtl8168(nic) => nic.send(frame),
        }
    }

    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Nic::Rtl8139(nic) => nic.receive(out),
            Nic::ThreeCom(nic) => nic.receive(out),
            Nic::E1000(nic) => nic.receive(out),
            Nic::E1000E(nic) => nic.receive(out),
            Nic::I350(nic) => nic.receive(out),
            Nic::Ne2000(nic) => nic.receive(out),
            Nic::Rtl8168(nic) => nic.receive(out),
        }
    }

    pub fn set_promiscuous(&self, enabled: bool) {
        match self {
            Nic::Rtl8139(nic) => nic.set_promiscuous(enabled),
            Nic::ThreeCom(nic) => nic.set_promiscuous(enabled),
            Nic::E1000(nic) => nic.set_promiscuous(enabled),
            Nic::E1000E(nic) => nic.set_promiscuous(enabled),
            Nic::I350(nic) => nic.set_promiscuous(enabled),
            Nic::Ne2000(nic) => nic.set_promiscuous(enabled),
            Nic::Rtl8168(nic) => nic.set_promiscuous(enabled),
        }
    }

    /// `nic regs`: a driver-specific register and ring dump, where implemented.
    pub fn dump_registers(&self) {
        match self {
            Nic::Rtl8168(nic) => nic.dump_registers(),
            Nic::ThreeCom(nic) => nic.dump_registers(),
            _ => println!("{}: no register dump implemented for this driver.", self.driver_name()),
        }
    }

    pub fn overflow_count(&self) -> u32 {
        match self {
            Nic::Rtl8139(nic) => nic.overflow_count(),
            Nic::ThreeCom(nic) => nic.overflow_count(),
            Nic::E1000(nic) => nic.overflow_count(),
            Nic::E1000E(nic) => nic.overflow_count(),
            Nic::I350(nic) => nic.overflow_count(),
            Nic::Ne2000(nic) => nic.overflow_count(),
            Nic::Rtl8168(nic) => nic.overflow_count(),
        }
    }

    pub fn interrupt_stats(&self) -> InterruptStats {
        let (irq, count, errors, pending) = match self {
            Nic::Rtl8139(nic) => nic.interrupt_stats(),
            Nic::ThreeCom(nic) => nic.interrupt_stats(),
            Nic::E1000(nic) => nic.interrupt_stats(),
            Nic::E1000E(nic) => nic.interrupt_stats(),
            Nic::I350(nic) => nic.interrupt_stats(),
            Nic::Ne2000(nic) => nic.interrupt_stats(),
            Nic::Rtl8168(nic) => nic.interrupt_stats(),
        };
        InterruptStats {
            irq,
            count,
            errors,
            pending,
        }
    }

    pub fn health(&self) -> Health {
        match self {
            Nic::Rtl8139(nic) => nic.health(),
            Nic::ThreeCom(nic) => nic.health(),
            Nic::E1000(nic) => nic.health(),
            Nic::E1000E(nic) => nic.health(),
            Nic::I350(nic) => nic.health(),
            Nic::Ne2000(nic) => nic.health(),
            Nic::Rtl8168(nic) => nic.health(),
        }
    }

    /// Returns `(speed_mbps, full_duplex)` if the link is currently up.
    /// Only implemented for `e1000e.rs` -- see `i350.rs`'s own removed
    /// `link_status` for why I350 doesn't have this too: reading its
    /// `STATUS` register at the classic `0x0008` offset (correct for
    /// e1000e, unconfirmed for I350's own substantially relocated
    /// register map) made real I350 hardware stop replying to `nic`/
    /// `nic list` entirely. Every other backend reports `None` rather
    /// than guessing at a register layout this project hasn't verified.
    pub fn link_status(&self) -> Option<(u16, bool)> {
        match self {
            Nic::E1000E(nic) => nic.link_status(),
            Nic::Rtl8168(nic) => nic.link_status(),
            _ => None,
        }
    }

    /// Runs a bounded, device-specific reset only after the driver has
    /// observed a documented fatal condition (such as a DMA overrun or a
    /// transmit completion timeout). It never infers a failure merely from
    /// an idle link: silence is normal for a raw Ethernet interface.
    pub fn watchdog(&self) -> WatchdogResult {
        match self {
            Nic::Rtl8139(nic) => nic.watchdog(),
            Nic::ThreeCom(nic) => nic.watchdog(),
            Nic::E1000(nic) => nic.watchdog(),
            Nic::E1000E(nic) => nic.watchdog(),
            Nic::I350(nic) => nic.watchdog(),
            Nic::Ne2000(nic) => nic.watchdog(),
            Nic::Rtl8168(nic) => nic.watchdog(),
        }
    }

    fn enable_interrupts(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.enable_interrupts(),
            Nic::ThreeCom(nic) => nic.enable_interrupts(),
            Nic::E1000(nic) => nic.enable_interrupts(),
            Nic::E1000E(nic) => nic.enable_interrupts(),
            Nic::I350(nic) => nic.enable_interrupts(),
            Nic::Ne2000(nic) => nic.enable_interrupts(),
            Nic::Rtl8168(nic) => nic.enable_interrupts(),
        }
    }

    fn handles_irq(&self, irq: u8) -> bool {
        self.interrupt_stats().irq == Some(irq)
    }

    fn handle_interrupt(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.handle_interrupt(),
            Nic::ThreeCom(nic) => nic.handle_interrupt(),
            Nic::E1000(nic) => nic.handle_interrupt(),
            Nic::E1000E(nic) => nic.handle_interrupt(),
            Nic::I350(nic) => nic.handle_interrupt(),
            Nic::Ne2000(nic) => nic.handle_interrupt(),
            Nic::Rtl8168(nic) => nic.handle_interrupt(),
        }
    }
}

/// Polls the selected card's health state. Callers naturally reach this from
/// shell status and normal send/receive traffic; it is deliberately never an
/// IRQ handler or timer task.
pub fn watchdog() -> Result<WatchdogResult, Status> {
    Ok(get()?.watchdog())
}

static mut NICS: [Option<Nic>; MAX_NICS] = [const { None }; MAX_NICS];
static mut NIC_COUNT: usize = 0;
static mut ACTIVE_INDEX: usize = 0;
/// True unless some initialized NIC lacks a registered, enabled receive
/// interrupt. Only then can a polling loop safely `hlt` between polls: the
/// NIC itself wakes the CPU for each arriving frame. A NIC without a usable
/// IRQ line would otherwise be polled only once per 10 ms PIT tick and could
/// overflow its small receive ring at wire speed. With no NIC at all there is
/// nothing to starve, so it stays true and keyboard-only loops may sleep too.
static mut ALL_RX_INTERRUPT_DRIVEN: bool = true;
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub enum Status {
    NotPresent,
    InitFailed(&'static str),
}

fn add(nic: Nic) {
    unsafe {
        if NIC_COUNT < MAX_NICS {
            NICS[NIC_COUNT] = Some(nic);
            NIC_COUNT += 1;
        }
    }
}

/// Initializes every supported backend instance once. Backend order remains
/// RTL8139, 3C905, e1000, e1000e, I350, NE2000-compatible, then RTL8168, so the
/// first initialized card preserves the previous default selection policy.
/// `i350::CANDIDATE_DEVICE_IDS` now carries one confirmed real device ID
/// (see `i350.rs`'s module doc comment), so this is no longer a guaranteed
/// no-op on hardware carrying that exact card -- its register-level
/// behavior is otherwise still unverified against physical hardware.
/// `e1000e::CANDIDATE_DEVICE_IDS` similarly carries one confirmed real
/// device ID (82579V) alongside QEMU's own 82574L model -- see
/// `e1000e.rs`'s module doc comment.
pub fn initialize() -> Result<usize, Status> {
    unsafe {
        if INITIALIZED {
            return if NIC_COUNT == 0 {
                Err(Status::NotPresent)
            } else {
                Ok(NIC_COUNT)
            };
        }
        INITIALIZED = true;
    }

    let mut init_failures = 0;
    match rtl8139::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = rtl8139::get(index) {
                    add(Nic::Rtl8139(nic));
                }
            }
        }
        Err(rtl8139::Status::NotPresent) => {}
        Err(rtl8139::Status::InitFailed) => init_failures += 1,
    }
    match threecom::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = threecom::get(index) {
                    add(Nic::ThreeCom(nic));
                }
            }
        }
        Err(threecom::Status::NotPresent) => {}
        Err(threecom::Status::InitFailed) => init_failures += 1,
    }
    match e1000::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = e1000::get(index) {
                    add(Nic::E1000(nic));
                }
            }
        }
        Err(e1000::Status::NotPresent) => {}
        Err(e1000::Status::InitFailed) => init_failures += 1,
    }
    match e1000e::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = e1000e::get(index) {
                    add(Nic::E1000E(nic));
                }
            }
        }
        Err(e1000e::Status::NotPresent) => {}
        Err(e1000e::Status::InitFailed) => init_failures += 1,
    }
    match i350::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = i350::get(index) {
                    add(Nic::I350(nic));
                }
            }
        }
        Err(i350::Status::NotPresent) => {}
        Err(i350::Status::InitFailed) => init_failures += 1,
    }
    match ne2000::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = ne2000::get(index) {
                    add(Nic::Ne2000(nic));
                }
            }
        }
        Err(ne2000::Status::NotPresent) => {}
        Err(ne2000::Status::InitFailed) => init_failures += 1,
    }
    match rtl8168::initialize_all() {
        Ok(count) => {
            for index in 0..count {
                if let Some(nic) = rtl8168::get(index) {
                    add(Nic::Rtl8168(nic));
                }
            }
        }
        Err(rtl8168::Status::NotPresent) => {}
        Err(rtl8168::Status::InitFailed) => init_failures += 1,
    }

    unsafe {
        if NIC_COUNT == 0 {
            if init_failures != 0 {
                Err(Status::InitFailed(
                    "one or more supported Ethernet controllers failed to initialize",
                ))
            } else {
                Err(Status::NotPresent)
            }
        } else {
            let mut all_driven = true;
            for index in 0..NIC_COUNT {
                let nic = NICS[index].as_ref().unwrap();
                let mut driven = false;
                if let Some(irq) = nic.interrupt_stats().irq {
                    if interrupts::register_pci_irq(irq, handle_pci_irq) {
                        nic.enable_interrupts();
                        driven = true;
                    }
                }
                all_driven &= driven;
            }
            ALL_RX_INTERRUPT_DRIVEN = all_driven;
            Ok(NIC_COUNT)
        }
    }
}

/// Shared legacy IRQs are normal. Each matching backend only reads and
/// acknowledges its own device status, leaving all frame parsing for callers'
/// ordinary polling receive loops.
fn handle_pci_irq(irq: u8) {
    unsafe {
        for index in 0..NIC_COUNT {
            if let Some(nic) = NICS[index].as_ref() {
                if nic.handles_irq(irq) {
                    nic.handle_interrupt();
                }
            }
        }
    }
}

/// Returns the currently selected interface, preserving the original
/// single-NIC callback behavior.
pub fn get() -> Result<&'static Nic, Status> {
    initialize()?;
    unsafe { Ok(NICS[ACTIVE_INDEX].as_ref().unwrap()) }
}

pub fn get_at(index: usize) -> Option<&'static Nic> {
    if initialize().is_err() {
        return None;
    }
    unsafe {
        if index >= NIC_COUNT {
            return None;
        }
        NICS[index].as_ref()
    }
}

pub fn count() -> usize {
    initialize().unwrap_or(0)
}

/// A NIC that has completed this many frames while its interrupt count is
/// still zero is assumed not to deliver receive interrupts. A working NIC
/// interrupts for (nearly) every frame, so even a handful of frames is
/// decisive; a quiet or unplugged NIC has completed none and is not judged.
const SUSPECT_RX_FRAMES: u32 = 8;

/// See `ALL_RX_INTERRUPT_DRIVEN`: whether a polling loop may halt between
/// polls without risking a missed or late frame. Registration alone is not
/// proof the interrupt arrives -- on the AMD/nForce test machine both 3C905s
/// register IRQ10 yet report zero dispatches while receiving frames -- so this
/// also requires that no NIC looks silent: completed frames (`rx_progress`,
/// counted by polling) with zero interrupt notifications mean sleeping would
/// delay each frame to the next 10 ms timer tick, so loops keep polling.
pub fn all_rx_interrupt_driven() -> bool {
    if !unsafe { ALL_RX_INTERRUPT_DRIVEN } {
        return false;
    }
    for index in 0..unsafe { NIC_COUNT } {
        if let Some(nic) = unsafe { NICS[index].as_ref() } {
            let stats = nic.interrupt_stats();
            if nic.health().rx_progress >= SUSPECT_RX_FRAMES && stats.count == 0 {
                return false;
            }
            // The storm guard masked this NIC's IRQ line: no interrupt will
            // ever wake a sleeping loop for it.
            if stats.irq.is_some_and(interrupts::pci_irq_storm_masked) {
                return false;
            }
        }
    }
    true
}

pub fn active_index() -> usize {
    unsafe { ACTIVE_INDEX }
}

pub fn set_active(index: usize) -> bool {
    if initialize().is_err() {
        return false;
    }
    unsafe {
        if index >= NIC_COUNT {
            return false;
        }
        ACTIVE_INDEX = index;
        true
    }
}
