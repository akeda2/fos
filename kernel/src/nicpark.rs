//! Parking network interfaces that have no cable: the copper PHY is put in power-down (IEEE 802.3 clause 22, basic mode
//! control register 0, bit 11), which switches off its transmitter and receiver and saves a fraction of a watt per port
//! (a hot box with several idle NICs adds up). The MAC and its DMA rings stay as they are.
//!
//! A powered-down PHY cannot see a cable plugged in, so a parked NIC is woken every `WAKE_EVERY_SECONDS` for
//! `WAKE_FOR_SECONDS` (autonegotiation takes two to three seconds): when the link comes up it stays unparked, otherwise
//! it goes back to sleep. A plugged cable is therefore noticed within about half a minute. Parking is by hand
//! (`nic park N`, `nic unpark N`) or, with `park-idle` in `settings`, automatic for a NIC whose link has been down for
//! `IDLE_SECONDS`. Only drivers that can reach the PHY take part (e1000 classic and I350, RTL8139, RTL8168); a NIC with a
//! link is never parked. QEMU models none of the power savings, so only real hardware shows the effect.

use crate::{nic::Nic, term, timer};

const POWER_DOWN: u16 = 1 << 11;
const AUTONEG_ENABLE: u16 = 1 << 12;
const RESTART_AUTONEG: u16 = 1 << 9;

const IDLE_SECONDS: u64 = 60;
const WAKE_EVERY_SECONDS: u64 = 30;
const WAKE_FOR_SECONDS: u64 = 8;

#[derive(Clone, Copy)]
struct Slot {
    parked: bool,
    /// Ticks when the link was first seen down (0: up, or not yet seen).
    down_since: u64,
    /// While parked: ticks of the next wake-up, and the end of the current one (0 when asleep).
    next_wake: u64,
    wake_until: u64,
    parks: u32,
}

static mut SLOTS: [Slot; term::MAX_NICS] = [Slot { parked: false, down_since: 0, next_wake: 0, wake_until: 0, parks: 0 }; term::MAX_NICS];
static mut AUTO: bool = false;

fn slot(index: usize) -> &'static mut Slot {
    unsafe { &mut (*core::ptr::addr_of_mut!(SLOTS))[index] }
}

/// `park-idle` in the settings file.
pub fn set_automatic() {
    unsafe { AUTO = true };
}

pub fn is_parked(index: usize) -> bool {
    index < term::MAX_NICS && slot(index).parked
}

fn power_down(nic: &mut Nic, down: bool) -> bool {
    let Some(control) = nic.phy_control() else { return false };
    let new = if down { control | POWER_DOWN } else { control & !POWER_DOWN | if control & AUTONEG_ENABLE != 0 { RESTART_AUTONEG } else { 0 } };
    nic.set_phy_control(new) && nic.phy_control().is_some_and(|read| (read & POWER_DOWN != 0) == down)
}

/// Parks NIC `index` (zero-based). The error says why not.
pub fn park(index: usize) -> Result<(), &'static str> {
    let nic = term::nic_at(index).ok_or("no such interface")?;
    if nic.phy_control().is_none() {
        return Err("this driver cannot reach the PHY");
    }
    if nic.link_up() {
        return Err("it has a link");
    }
    if !power_down(nic, true) {
        return Err("the PHY did not power down");
    }
    let slot = slot(index);
    slot.parked = true;
    slot.parks += 1;
    slot.wake_until = 0;
    slot.next_wake = timer::ticks() + WAKE_EVERY_SECONDS * timer::TICK_HZ;
    Ok(())
}

pub fn unpark(index: usize) -> Result<(), &'static str> {
    let nic = term::nic_at(index).ok_or("no such interface")?;
    if !slot(index).parked {
        return Err("it is not parked");
    }
    power_down(nic, false);
    let slot = slot(index);
    slot.parked = false;
    slot.down_since = 0;
    Ok(())
}

/// Called from the periodic service loops: parks idle NICs (automatic mode) and runs the wake-up checks of parked ones.
pub fn service() {
    let now = timer::ticks();
    for index in 0..term::nic_count() {
        let Some(nic) = term::nic_at(index) else { continue };
        let state = slot(index);
        if state.parked {
            if state.wake_until != 0 {
                if nic.link_up() {
                    state.parked = false;
                    state.wake_until = 0;
                    state.down_since = 0;
                } else if now >= state.wake_until {
                    state.wake_until = 0;
                    state.next_wake = now + WAKE_EVERY_SECONDS * timer::TICK_HZ;
                    power_down(nic, true);
                }
            } else if now >= state.next_wake && power_down(nic, false) {
                state.wake_until = now + WAKE_FOR_SECONDS * timer::TICK_HZ;
            }
        } else if unsafe { AUTO } {
            if nic.link_up() {
                state.down_since = 0;
            } else if state.down_since == 0 {
                state.down_since = now.max(1);
            } else if now - state.down_since >= IDLE_SECONDS * timer::TICK_HZ {
                let _ = park(index);
                state.down_since = now;
            }
        }
    }
}

/// The state line `nic` prints for an interface.
pub fn describe(index: usize) {
    let state = slot(index);
    if state.parked {
        crate::println!("   PHY parked (powered down, checked every {WAKE_EVERY_SECONDS} s for a cable); `nic unpark {}` wakes it", index + 1);
    } else if state.parks != 0 {
        crate::println!("   PHY parked {} time(s) so far", state.parks);
    }
}
