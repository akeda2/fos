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

    /// Negotiated (Mbit/s, full duplex), where the driver can tell.
    pub fn link_speed(&self) -> Option<(u16, bool)> {
        match self {
            Nic::E1000(nic) => nic.link_speed(),
            Nic::Rtl8168(nic) => nic.link_speed(),
            _ => None,
        }
    }

    /// Missed packets, no-buffer events, CRC errors, symbol errors, PAUSE frames received and sent (running totals), for
    /// `TapeApi::nic_counters`; e1000 family only.
    pub fn error_counters(&self) -> Option<[u64; 6]> {
        match self {
            Nic::E1000(nic) => Some(nic.error_counters()),
            _ => None,
        }
    }

    /// An MII register of the copper PHY, for `nic phy` (e1000 classic and I350 only).
    pub fn phy_read(&self, register: u32) -> Option<u16> {
        match self {
            Nic::E1000(nic) => nic.phy_read(register),
            _ => None,
        }
    }

    /// The PHY's basic mode control register, for drivers that can reach the PHY (see `nicpark`).
    pub fn phy_control(&self) -> Option<u16> {
        match self {
            Nic::E1000(nic) => nic.phy_control(),
            Nic::Rtl8139(nic) => nic.phy_control(),
            Nic::Rtl8168(nic) => nic.phy_control(),
            Nic::ThreeCom(_) => None,
        }
    }

    pub fn set_phy_control(&mut self, value: u16) -> bool {
        match self {
            Nic::E1000(nic) => nic.set_phy_control(value),
            Nic::Rtl8139(nic) => nic.set_phy_control(value),
            Nic::Rtl8168(nic) => nic.set_phy_control(value),
            Nic::ThreeCom(_) => false,
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

    /// The hardware receive timestamp (nanoseconds of the card's clock) of the frame `receive` returned last; 0 if none.
    pub fn take_stamp(&mut self) -> u64 {
        match self {
            Nic::E1000(nic) => nic.take_stamp(),
            _ => 0,
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

    /// The e1000-family driver when this is one (for the IEEE 1588 timestamp calls, `nic ts`).
    pub fn as_e1000(&mut self) -> Option<&mut crate::e1000::E1000> {
        match self {
            Nic::E1000(nic) => Some(nic),
            _ => None,
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

    /// Joins or leaves one multicast group in the hardware filter (`TapeApi::nic_multicast_join`); false on a card
    /// without one (the caller then opens all multicast with `set_multicast`).
    pub fn multicast_join(&mut self, mac: [u8; 6], on: bool) -> bool {
        match self {
            Nic::E1000(nic) => nic.multicast_join(mac, on),
            _ => false,
        }
    }

    pub fn multicast_clear(&mut self) {
        if let Nic::E1000(nic) = self {
            nic.multicast_clear();
        }
    }

    /// Transmit checksum offload (`TapeApi::nic_tx_offload`); false on a card without it.
    pub fn set_tx_offload(&mut self, on: bool) -> bool {
        match self {
            Nic::E1000(nic) => nic.set_tx_offload(on),
            _ => false,
        }
    }

    /// Registers or drops one VLAN id in the hardware VLAN filter (`TapeApi::nic_vlan_filter`); false on a card without one.
    pub fn vlan_filter(&mut self, vid: u16, on: bool) -> bool {
        match self {
            Nic::E1000(nic) => nic.vlan_filter(vid, on),
            _ => false,
        }
    }

    pub fn vlan_clear(&mut self) {
        if let Nic::E1000(nic) = self {
            nic.vlan_clear();
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
            Nic::E1000(nic) => nic.describe(),
        }
    }

    /// `nic regs`: a register dump, for drivers that have one.
    pub fn dump_registers(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.dump_registers(),
            Nic::Rtl8168(nic) => nic.dump_registers(),
            Nic::ThreeCom(nic) => nic.dump_registers(),
            Nic::E1000(nic) => nic.dump_registers(),
        }
    }

}
