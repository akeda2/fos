//! The network interfaces the shell and tapes can use, whichever driver backs them. Everything
//! above the drivers (the terminal protocol, the tape ABI) talks to this enum; each driver keeps
//! its own struct and register logic.

use crate::e1000::{Delivery, E1000};
use crate::rtl8168::Rtl8168;

pub enum Nic {
    E1000(E1000),
    Rtl8168(Rtl8168),
}

impl Nic {
    pub fn mac(&self) -> [u8; 6] {
        match self {
            Nic::E1000(nic) => nic.mac,
            Nic::Rtl8168(nic) => nic.mac,
        }
    }

    /// The driver family as `nic` lists it.
    pub fn family(&self) -> &'static str {
        match self {
            Nic::E1000(nic) => match nic.variant {
                crate::e1000::Variant::Classic => "e1000",
                crate::e1000::Variant::E1000e => "e1000e",
                crate::e1000::Variant::I350 => "i350",
            },
            Nic::Rtl8168(_) => "rtl8168",
        }
    }

    pub fn link_up(&self) -> bool {
        match self {
            Nic::E1000(nic) => nic.link_up(),
            Nic::Rtl8168(nic) => nic.link_up(),
        }
    }

    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        match self {
            Nic::E1000(nic) => nic.receive(out),
            Nic::Rtl8168(nic) => nic.receive(out),
        }
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        match self {
            Nic::E1000(nic) => nic.send(frame),
            Nic::Rtl8168(nic) => nic.send(frame),
        }
    }

    pub fn set_promiscuous(&mut self, on: bool) {
        match self {
            Nic::E1000(nic) => nic.set_promiscuous(on),
            Nic::Rtl8168(nic) => nic.set_promiscuous(on),
        }
    }

    pub fn delivery(&self) -> Delivery {
        match self {
            Nic::E1000(nic) => nic.delivery,
            Nic::Rtl8168(nic) => nic.delivery,
        }
    }

    /// Frames received, sent, and dropped because the link was down.
    pub fn counters(&self) -> (u64, u64, u64) {
        match self {
            Nic::E1000(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::Rtl8168(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
        }
    }

    /// A driver-specific line for `nic` (nothing for the e1000 family, whose counters are shared).
    pub fn describe(&self) {
        if let Nic::Rtl8168(nic) = self {
            nic.describe();
        }
    }

    /// `nic regs`: a register dump, for drivers that have one.
    pub fn dump_registers(&self) {
        match self {
            Nic::Rtl8168(nic) => nic.dump_registers(),
            Nic::E1000(_) => crate::println!("   (no register dump for this driver)"),
        }
    }

    /// The e1000 interrupt bookkeeping index; only e1000-family NICs have any.
    pub fn is_e1000(&self) -> bool {
        matches!(self, Nic::E1000(_))
    }
}
