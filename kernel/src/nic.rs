//! Common kernel-owned Ethernet facade.
//!
//! Networking tapes see one stable raw-Ethernet callback contract. This
//! facade initializes every supported PCI backend instance, keeps the first
//! initialized card active by default, and lets shell commands and newer
//! tapes select a specific interface without exposing PCI or DMA details.
//! Device IRQs only acknowledge a cause and make a foreground poll prompt;
//! all descriptor and protocol parsing remains in foreground code.

use crate::{e1000, interrupts, pci, rtl8139, threecom};

pub const MAX_FRAME_LEN: usize = 1514;
const MAX_NICS: usize = pci::MAX_DEVICE_MATCHES * 3;

pub enum Nic {
    Rtl8139(&'static rtl8139::Rtl8139),
    ThreeCom(&'static threecom::ThreeCom),
    E1000(&'static e1000::E1000),
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
        }
    }

    pub fn mac_address(&self) -> [u8; 6] {
        match self {
            Nic::Rtl8139(nic) => nic.mac_address(),
            Nic::ThreeCom(nic) => nic.mac_address(),
            Nic::E1000(nic) => nic.mac_address(),
        }
    }

    pub fn link_up(&self) -> Option<bool> {
        match self {
            Nic::Rtl8139(_) | Nic::E1000(_) => None,
            Nic::ThreeCom(nic) => Some(nic.link_up()),
        }
    }

    pub fn send(&self, frame: &[u8]) -> bool {
        match self {
            Nic::Rtl8139(nic) => nic.send(frame),
            Nic::ThreeCom(nic) => nic.send(frame),
            Nic::E1000(nic) => nic.send(frame),
        }
    }

    pub fn receive(&self, out: &mut [u8]) -> Option<usize> {
        match self {
            Nic::Rtl8139(nic) => nic.receive(out),
            Nic::ThreeCom(nic) => nic.receive(out),
            Nic::E1000(nic) => nic.receive(out),
        }
    }

    pub fn set_promiscuous(&self, enabled: bool) {
        match self {
            Nic::Rtl8139(nic) => nic.set_promiscuous(enabled),
            Nic::ThreeCom(nic) => nic.set_promiscuous(enabled),
            Nic::E1000(nic) => nic.set_promiscuous(enabled),
        }
    }

    pub fn overflow_count(&self) -> u32 {
        match self {
            Nic::Rtl8139(nic) => nic.overflow_count(),
            Nic::ThreeCom(nic) => nic.overflow_count(),
            Nic::E1000(nic) => nic.overflow_count(),
        }
    }

    pub fn interrupt_stats(&self) -> InterruptStats {
        let (irq, count, errors, pending) = match self {
            Nic::Rtl8139(nic) => nic.interrupt_stats(),
            Nic::ThreeCom(nic) => nic.interrupt_stats(),
            Nic::E1000(nic) => nic.interrupt_stats(),
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
        }
    }

    fn enable_interrupts(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.enable_interrupts(),
            Nic::ThreeCom(nic) => nic.enable_interrupts(),
            Nic::E1000(nic) => nic.enable_interrupts(),
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
/// RTL8139, 3C905, then e1000, so the first initialized card preserves the
/// previous default selection policy.
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
            for index in 0..NIC_COUNT {
                let nic = NICS[index].as_ref().unwrap();
                if let Some(irq) = nic.interrupt_stats().irq {
                    if interrupts::register_pci_irq(irq, handle_pci_irq) {
                        nic.enable_interrupts();
                    }
                }
            }
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
