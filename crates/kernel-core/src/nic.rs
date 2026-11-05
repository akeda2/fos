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
    Virtual(Virtual),
}

/// A virtual interface: its own MAC on a physical NIC (`parent`). The parent does layer 2: `nicctl::drain_nic_to_fifo` hands
/// the frames addressed to `mac` to this interface's queue and `send` goes out of the parent. A removed one stays in the table
/// as a dead slot (`live == false`, no link) so interface numbers never shift; `add_virtual` reuses it.
pub struct Virtual {
    pub parent: usize,
    pub mac: [u8; 6],
    pub live: bool,
    pub frames_received: u64,
    pub frames_sent: u64,
}

impl Nic {
    pub fn mac(&self) -> [u8; 6] {
        match self {
            Nic::E1000(nic) => nic.mac,
            Nic::Rtl8139(nic) => nic.mac,
            Nic::Rtl8168(nic) => nic.mac,
            Nic::ThreeCom(nic) => nic.mac,
            Nic::Virtual(nic) => nic.mac,
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
            Nic::Virtual(_) => "vnic",
        }
    }

    pub fn link_up(&self) -> bool {
        match self {
            Nic::E1000(nic) => nic.link_up(),
            Nic::Rtl8139(nic) => nic.link_up(),
            Nic::Rtl8168(nic) => nic.link_up(),
            Nic::ThreeCom(nic) => nic.link_up(),
            Nic::Virtual(nic) => nic.live && parent_of(nic).is_some_and(|parent| parent.link_up()),
        }
    }

    /// Negotiated (Mbit/s, full duplex), where the driver can tell.
    pub fn link_speed(&self) -> Option<(u16, bool)> {
        match self {
            Nic::E1000(nic) => nic.link_speed(),
            Nic::Rtl8168(nic) => nic.link_speed(),
            Nic::Virtual(nic) => parent_of(nic).and_then(|parent| parent.link_speed()),
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
            Nic::ThreeCom(_) | Nic::Virtual(_) => None,
        }
    }

    pub fn set_phy_control(&mut self, value: u16) -> bool {
        match self {
            Nic::E1000(nic) => nic.set_phy_control(value),
            Nic::Rtl8139(nic) => nic.set_phy_control(value),
            Nic::Rtl8168(nic) => nic.set_phy_control(value),
            Nic::ThreeCom(_) | Nic::Virtual(_) => false,
        }
    }

    /// A virtual interface receives nothing from hardware: the parent's drain queues its frames.
    pub fn receive(&mut self, out: &mut [u8]) -> Option<usize> {
        match self {
            Nic::E1000(nic) => nic.receive(out),
            Nic::Rtl8139(nic) => nic.receive(out),
            Nic::Rtl8168(nic) => nic.receive(out),
            Nic::ThreeCom(nic) => nic.receive(out),
            Nic::Virtual(_) => None,
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
            Nic::Virtual(nic) => {
                let sent = nic.live && parent_of(nic).is_some_and(|parent| parent.send(frame));
                nic.frames_sent += u64::from(sent);
                sent
            }
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
            Nic::Virtual(_) => {} // the parent is promiscuous for as long as virtual interfaces use it
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
            Nic::Rtl8168(nic) => nic.set_tx_offload(on),
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
            Nic::Virtual(_) => {} // the promiscuous parent delivers every multicast frame
        }
    }

    pub fn delivery(&self) -> Delivery {
        match self {
            Nic::E1000(nic) => nic.delivery,
            Nic::Rtl8139(nic) => nic.delivery,
            Nic::Rtl8168(nic) => nic.delivery,
            Nic::ThreeCom(nic) => nic.delivery,
            Nic::Virtual(nic) => parent_of(nic).map_or(Delivery::Polled, |parent| parent.delivery()),
        }
    }

    /// Frames received, sent, and dropped because the link was down.
    pub fn counters(&self) -> (u64, u64, u64) {
        match self {
            Nic::E1000(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::Rtl8139(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::Rtl8168(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::ThreeCom(nic) => (nic.frames_received, nic.frames_sent, nic.frames_dropped_no_link),
            Nic::Virtual(nic) => (nic.frames_received, nic.frames_sent, 0),
        }
    }

    /// A driver-specific line for `nic` (nothing for the e1000 family, whose counters are shared).
    pub fn describe(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.describe(),
            Nic::Rtl8168(nic) => nic.describe(),
            Nic::ThreeCom(nic) => nic.describe(),
            Nic::E1000(nic) => nic.describe(),
            Nic::Virtual(nic) if nic.live => crate::println!("   virtual interface on nic{}", nic.parent + 1),
            Nic::Virtual(_) => crate::println!("   virtual interface, removed"),
        }
    }

    /// `nic regs`: a register dump, for drivers that have one.
    pub fn dump_registers(&self) {
        match self {
            Nic::Rtl8139(nic) => nic.dump_registers(),
            Nic::Rtl8168(nic) => nic.dump_registers(),
            Nic::ThreeCom(nic) => nic.dump_registers(),
            Nic::E1000(nic) => nic.dump_registers(),
            Nic::Virtual(_) => {}
        }
    }
}

fn parent_of(nic: &Virtual) -> Option<&'static mut Nic> {
    at(nic.parent)
}

/// How many interfaces the machine can have in use at once.
/// (16 with the `vnic` feature of the server kernel, which adds virtual interfaces; the queues are static.)
pub const MAX_NICS: usize = if cfg!(feature = "vnic") { 16 } else { 8 };
static mut NICS: [Option<Nic>; MAX_NICS] = [const { None }; MAX_NICS];
static mut NIC_COUNT: usize = 0;

/// Adds a network interface; returns its zero-based index (`None` when the table is full).
pub fn install(nic: Nic) -> Option<usize> {
    unsafe {
        if NIC_COUNT == MAX_NICS {
            return None;
        }
        (*core::ptr::addr_of_mut!(NICS))[NIC_COUNT] = Some(nic);
        NIC_COUNT += 1;
        Some(NIC_COUNT - 1)
    }
}

pub fn count() -> usize {
    unsafe { NIC_COUNT }
}

pub fn at(index: usize) -> Option<&'static mut Nic> {
    unsafe { (*core::ptr::addr_of_mut!(NICS)).get_mut(index)?.as_mut() }
}

/// True when every interface delivers interrupts (MSI), so a frame wakes the CPU and a tape may sleep in `hlt` instead of
/// polling. One polled NIC is enough to say no: its frames would only be noticed at the next timer tick.
pub fn all_interrupt_driven() -> bool {
    let count = count();
    count > 0 && (0..count).all(|index| at(index).is_some_and(|nic| nic.delivery() != Delivery::Polled))
}

/// Adds a virtual interface with `mac` on physical interface `parent` (a dead slot is reused); returns its index. The parent
/// goes promiscuous so the card delivers frames for the new MAC. `None` when `parent` is not a physical NIC, the table is full
/// or another interface already has that MAC.
pub fn add_virtual(parent: usize, mac: [u8; 6]) -> Option<usize> {
    if at(parent).is_none_or(|nic| matches!(nic, Nic::Virtual(_))) || (0..count()).any(|index| at(index).is_some_and(|nic| nic.mac() == mac && !matches!(nic, Nic::Virtual(Virtual { live: false, .. })))) {
        return None;
    }
    let fresh = Virtual { parent, mac, live: true, frames_received: 0, frames_sent: 0 };
    let index = match (0..count()).find(|&index| matches!(at(index), Some(Nic::Virtual(Virtual { live: false, .. })))) {
        Some(index) => {
            *at(index)? = Nic::Virtual(fresh);
            index
        }
        None => install(Nic::Virtual(fresh))?,
    };
    at(parent)?.set_promiscuous(true);
    Some(index)
}

/// Removes virtual interface `index` (its slot stays, dead). The parent leaves promiscuous mode with its last one.
pub fn remove_virtual(index: usize) -> bool {
    let Some(Nic::Virtual(nic)) = at(index) else { return false };
    if !nic.live {
        return false;
    }
    nic.live = false;
    let parent = nic.parent;
    if !has_virtual(parent) {
        if let Some(card) = at(parent) {
            card.set_promiscuous(false);
        }
    }
    true
}

/// True when `index` is a virtual interface (live or removed).
pub fn is_virtual(index: usize) -> bool {
    matches!(at(index), Some(Nic::Virtual(_)))
}

/// The physical interface a live virtual interface sits on.
pub fn virtual_parent(index: usize) -> Option<usize> {
    match at(index) {
        Some(Nic::Virtual(nic)) if nic.live => Some(nic.parent),
        _ => None,
    }
}

/// True when a live virtual interface sits on `parent`.
pub fn has_virtual(parent: usize) -> bool {
    (0..count()).any(|index| virtual_parent(index) == Some(parent))
}

/// The live virtual interfaces on `parent` (index, MAC), for the receive demux; returns how many it filled in.
pub fn virtual_children(parent: usize, out: &mut [(usize, [u8; 6])]) -> usize {
    let mut found = 0;
    for index in 0..count() {
        if virtual_parent(index) == Some(parent) && found < out.len() {
            out[found] = (index, at(index).map_or([0; 6], |nic| nic.mac()));
            found += 1;
        }
    }
    found
}
