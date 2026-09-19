//! Common kernel-owned Ethernet facade.
//!
//! Networking tapes see one stable raw-Ethernet callback contract. This
//! facade initializes every supported PCI backend instance, keeps the first
//! initialized card active by default, and lets shell commands and newer
//! tapes select a specific interface without exposing PCI or DMA details.

use crate::{e1000, pci, rtl8139, threecom};

pub const MAX_FRAME_LEN: usize = 1514;
const MAX_NICS: usize = pci::MAX_DEVICE_MATCHES * 3;

pub enum Nic {
    Rtl8139(&'static rtl8139::Rtl8139),
    ThreeCom(&'static threecom::ThreeCom),
    E1000(&'static e1000::E1000),
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
            Ok(NIC_COUNT)
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
