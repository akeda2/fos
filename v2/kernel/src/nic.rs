//! The network interfaces the shell and tapes can use, whichever driver backs them. Everything
//! above the drivers (the terminal protocol, the tape ABI) talks to this enum; each driver keeps
//! its own struct and register logic.

use crate::e1000::{Delivery, E1000};
use crate::rtl8139::Rtl8139;
use crate::rtl8168::Rtl8168;
use crate::threecom::ThreeCom;

pub enum Nic {
    E1000(E1000),
    Rtl8139(Rtl8139),
    Rtl8168(Rtl8168),
    ThreeCom(ThreeCom),
}

impl Nic {
    pub fn mac(&self) -> [u8; 6] {
        match self {
            Nic::E1000(nic) => nic.mac,
            Nic::Rtl8139(nic) => nic.mac,
            Nic::Rtl8168(nic) => nic.mac,
            Nic::ThreeCom(nic) => nic.mac,
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
            Nic::Rtl8139(_) => "rtl8139",
            Nic::Rtl8168(_) => "rtl8168",
            Nic::ThreeCom(_) => "3c905",
        }
    }

    pub fn link_up(&self) -> bool {
        match self {
            Nic::E1000(nic) => nic.link_up(),
            Nic::Rtl8139(nic) => nic.link_up(),
            Nic::Rtl8168(nic) => nic.link_up(),
            Nic::ThreeCom(nic) => nic.link_up(),
        }
    }

    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        match self {
            Nic::E1000(nic) => nic.receive(out),
            Nic::Rtl8139(nic) => nic.receive(out),
            Nic::Rtl8168(nic) => nic.receive(out),
            Nic::ThreeCom(nic) => nic.receive(out),
        }
    }

    pub fn send(&mut self, frame: &[u8]) -> bool {
        match self {
            Nic::E1000(nic) => nic.send(frame),
            Nic::Rtl8139(nic) => nic.send(frame),
            Nic::Rtl8168(nic) => nic.send(frame),
            Nic::ThreeCom(nic) => nic.send(frame),
        }
    }

    pub fn set_promiscuous(&mut self, on: bool) {
        match self {
            Nic::E1000(nic) => nic.set_promiscuous(on),
            Nic::Rtl8139(nic) => nic.set_promiscuous(on),
            Nic::Rtl8168(nic) => nic.set_promiscuous(on),
            Nic::ThreeCom(nic) => nic.set_promiscuous(on),
        }
    }

    /// Accept all multicast frames (see `TapeApi::nic_set_multicast`).
    pub fn set_multicast(&mut self, on: bool) {
        match self {
            Nic::E1000(nic) => nic.set_multicast(on),
            Nic::Rtl8139(nic) => nic.set_multicast(on),
            Nic::Rtl8168(_) => {} // already accepts every multicast frame (its hash filter is all ones)
            Nic::ThreeCom(nic) => nic.set_multicast(on),
        }
    }

    pub fn delivery(&self) -> Delivery {
        match self {
            Nic::E1000(nic) => nic.delivery,
            Nic::Rtl8139(nic) => nic.delivery,
            Nic::Rtl8168(nic) => nic.delivery,
            Nic::ThreeCom(nic) => nic.delivery,
        }
    }

    /// Frames received, sent, and dropped because the link was down.
    pub fn counters(&self) -> (u64, u64, u64) {
        match self {
            Nic::E1000(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::Rtl8139(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::Rtl8168(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::ThreeCom(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
        }
    }

    /// A driver-specific line for `nic` (nothing for the e1000 family, whose counters are shared).
    pub fn describe(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.describe(),
            Nic::Rtl8168(nic) => nic.describe(),
            Nic::ThreeCom(nic) => nic.describe(),
            Nic::E1000(_) => {}
        }
    }

    /// `nic regs`: a register dump, for drivers that have one.
    pub fn dump_registers(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.dump_registers(),
            Nic::Rtl8168(nic) => nic.dump_registers(),
            Nic::ThreeCom(nic) => nic.dump_registers(),
            Nic::E1000(_) => crate::println!("   (no register dump for this driver)"),
        }
    }

    /// The e1000 interrupt bookkeeping index; only e1000-family NICs have any.
    pub fn is_e1000(&self) -> bool {
        matches!(self, Nic::E1000(_))
    }
}
