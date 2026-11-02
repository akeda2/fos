//! Per-interface receive FIFOs for tapes, and the small NIC control calls the tape ABI forwards (timestamps, multicast, VLAN,
//! offload, counters). They sit on the NIC table in `nic.rs`; the terminal protocol that fills the FIFOs stays in the kernel.

use crate::nic::{self, MAX_NICS};

/// Frames that were not the terminal's, kept per interface for the tape's `nic_receive`
/// (a FIFO twice the hardware ring: a tape that does not poll loses the oldest traffic, like a NIC ring
/// overrun; 64 slots made `nettest` lose small frames long before the NIC did).
const FRAME_FIFO: usize = 256;
static mut FRAMES: [[u8; 1600]; MAX_NICS * FRAME_FIFO] = [[0; 1600]; MAX_NICS * FRAME_FIFO];
static mut FRAME_LENGTHS: [usize; MAX_NICS * FRAME_FIFO] = [0; MAX_NICS * FRAME_FIFO];
/// Hardware receive stamp of each queued frame (0: none), and of the frame `pop_frame` handed out last.
static mut FRAME_STAMPS: [u64; MAX_NICS * FRAME_FIFO] = [0; MAX_NICS * FRAME_FIFO];
static mut LAST_STAMP: [u64; MAX_NICS] = [0; MAX_NICS];
static mut FRAME_HEAD: [usize; MAX_NICS] = [0; MAX_NICS];
static mut FRAME_TAIL: [usize; MAX_NICS] = [0; MAX_NICS];
static mut FRAMES_DROPPED: [u32; MAX_NICS] = [0; MAX_NICS];
static mut FRAMES_QUEUED: u32 = 0;

/// Frames queued for tapes and frames dropped (all interfaces).
pub fn frame_queue_counts() -> (u32, u32) {
    unsafe { (FRAMES_QUEUED, (*core::ptr::addr_of!(FRAMES_DROPPED)).iter().sum::<u32>()) }
}

/// True when interface `index` has a queued frame.
pub fn frame_pending(index: usize) -> bool {
    index < MAX_NICS && unsafe { FRAME_TAIL[index] != FRAME_HEAD[index] }
}

/// Keeps a frame that is not the terminal's for a tape (`tape load net` listens for its own
/// protocol, `nic_receive` for everything else). Frames that arrive before a tape is
/// listening wait here instead of being lost.
pub fn queue_frame(index: usize, frame: &[u8], stamp: u64) {
    unsafe {
        let head = FRAME_HEAD[index];
        let next = (head + 1) % FRAME_FIFO;
        if next == FRAME_TAIL[index] {
            FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO; // full: drop the oldest
            FRAMES_DROPPED[index] += 1;
        }
        let slot = index * FRAME_FIFO + head;
        core::slice::from_raw_parts_mut((core::ptr::addr_of_mut!(FRAMES) as *mut u8).add(slot * 1600), frame.len()).copy_from_slice(frame);
        FRAME_LENGTHS[slot] = frame.len();
        FRAME_STAMPS[slot] = stamp;
        FRAME_HEAD[index] = next;
        FRAMES_QUEUED += 1;
    }
}

/// Forgets frames queued for tapes (a tape about to run should not see stale traffic).
pub fn clear_frame_queue() {
    for index in 0..MAX_NICS {
        if !crate::tasks::service_owns(index) {
            clear_frame_queue_nic(index);
        }
    }
}

pub fn clear_frame_queue_nic(index: usize) {
    if index < MAX_NICS {
        unsafe {
            FRAME_HEAD[index] = 0;
            FRAME_TAIL[index] = 0;
        }
    }
}

/// The oldest queued non-terminal frame of interface `index`, copied into `out`.
pub fn pop_frame(index: usize, out: &mut [u8]) -> usize {
    if index >= MAX_NICS {
        return 0;
    }
    unsafe {
        if FRAME_TAIL[index] == FRAME_HEAD[index] {
            return 0;
        }
        let slot = index * FRAME_FIFO + FRAME_TAIL[index];
        let length = FRAME_LENGTHS[slot].min(out.len());
        out[..length].copy_from_slice(core::slice::from_raw_parts((core::ptr::addr_of!(FRAMES) as *const u8).add(slot * 1600), length));
        LAST_STAMP[index] = FRAME_STAMPS[slot];
        FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO;
        length
    }
}

/// The hardware receive stamp of the frame `pop_frame` (so a tape's `nic_receive`) returned last on `index`; 0 if none.
pub fn last_rx_stamp(index: usize) -> u64 {
    if index >= MAX_NICS { 0 } else { unsafe { LAST_STAMP[index] } }
}

/// A service ended: its NIC goes back to the state a tape leaves it in.
pub fn nic_release(index: usize) {
    if let Some(nic) = nic::at(index) {
        nic.multicast_clear();
        nic.vlan_clear();
        nic.set_tx_offload(false);
        if let Some(card) = nic.as_e1000() {
            if card.ts_all() {
                card.ts_stamp_all(false);
            }
        }
    }
}

/// Arms hardware timestamps on interface `index` (index 0-based); false if the card has none.
pub fn nic_ts_enable(index: usize) -> bool {
    nic::at(index).and_then(|nic| nic.as_e1000()).is_some_and(|card| card.ts_enable())
}

/// The card's clock in nanoseconds (0 without timestamps).
pub fn nic_ts_clock(index: usize) -> u64 {
    nic::at(index).and_then(|nic| nic.as_e1000()).filter(|card| card.ts_supported()).map_or(0, |card| card.systim())
}

/// Frequency-adjusts the card's clock by `ppb` (false without timestamps).
pub fn nic_ts_adjust(index: usize, ppb: i32) -> bool {
    nic::at(index).and_then(|nic| nic.as_e1000()).filter(|card| card.ts_supported()).map(|card| card.ts_adjust(ppb)).is_some()
}

/// Stamps every received frame (`on`) or only PTP events again; false without timestamps armed.
pub fn nic_ts_all(index: usize, on: bool) -> bool {
    nic::at(index).and_then(|nic| nic.as_e1000()).is_some_and(|card| card.ts_stamp_all(on))
}

/// Back to stamping only PTP events on every interface (after a tape that asked for every frame).
pub fn nic_ts_all_off() {
    for index in 0..nic::count() {
        if crate::tasks::service_owns(index) {
            continue;
        }
        if let Some(card) = nic::at(index).and_then(|nic| nic.as_e1000()) {
            if card.ts_all() {
                card.ts_stamp_all(false);
            }
        }
    }
}

/// Steps the card's clock by `delta` nanoseconds (false without timestamps).
pub fn nic_ts_step(index: usize, delta: i64) -> bool {
    nic::at(index).and_then(|nic| nic.as_e1000()).filter(|card| card.ts_supported()).map(|card| card.ts_step(delta)).is_some()
}

/// Sends `frame` and returns (sent, transmit stamp); the stamp is 0 when the card has none.
pub fn nic_send_stamped(index: usize, frame: &[u8]) -> (bool, u64) {
    let Some(nic) = nic::at(index) else { return (false, 0) };
    match nic.as_e1000().filter(|card| card.ts_supported()) {
        Some(card) => card.send_stamped(frame),
        None => (nic.send(frame), 0),
    }
}

pub fn nic_mac(index: usize, out: &mut [u8; 6]) -> bool {
    match nic::at(index) {
        Some(nic) => {
            *out = nic.mac();
            true
        }
        None => false,
    }
}

pub fn nic_send(index: usize, frame: &[u8]) -> bool {
    nic::at(index).is_some_and(|nic| nic.send(frame))
}

pub fn nic_set_promiscuous(index: usize, on: bool) -> bool {
    match nic::at(index) {
        Some(nic) => {
            nic.set_promiscuous(on);
            true
        }
        None => false,
    }
}

pub fn nic_multicast_join(index: usize, mac: [u8; 6], on: bool) -> bool {
    nic::at(index).is_some_and(|nic| nic.multicast_join(mac, on))
}

/// Drops every hardware multicast group on every NIC (a tape ended; the shell wants none).
pub fn nic_multicast_clear_all() {
    let mut index = 0;
    while let Some(nic) = nic::at(index) {
        if !crate::tasks::service_owns(index) {
            nic.multicast_clear();
        }
        index += 1;
    }
}

pub fn nic_vlan_filter(index: usize, vid: u16, on: bool) -> bool {
    nic::at(index).is_some_and(|nic| nic.vlan_filter(vid, on))
}

/// Drops every VLAN id (filter off) on every NIC (a tape ended).
pub fn nic_vlan_clear_all() {
    let mut index = 0;
    while let Some(nic) = nic::at(index) {
        if !crate::tasks::service_owns(index) {
            nic.vlan_clear();
        }
        index += 1;
    }
}

/// `TapeApi::nic_counters`: fills `out` with negotiated Mbit/s (0 unknown), full duplex (0/1), the six card error
/// totals (`Nic::error_counters`) and frames the kernel's tape queue dropped; returns how many it wrote.
pub fn nic_counters(index: usize, out: &mut [u64]) -> usize {
    let Some(nic) = nic::at(index) else { return 0 };
    let (speed, duplex) = nic.link_speed().map_or((0, 0), |(speed, full)| (u64::from(speed), u64::from(full)));
    let mut all = [0u64; 9];
    all[0] = speed;
    all[1] = duplex;
    if let Some(errors) = nic.error_counters() {
        all[2..8].copy_from_slice(&errors);
    }
    all[8] = u64::from(unsafe { FRAMES_DROPPED[index] });
    let count = out.len().min(all.len());
    out[..count].copy_from_slice(&all[..count]);
    count
}

pub fn nic_tx_offload(index: usize, on: bool) -> bool {
    nic::at(index).is_some_and(|nic| nic.set_tx_offload(on))
}

/// Turns transmit checksum offload off on every NIC (a tape ended; ordinary frames carry their own checksums).
pub fn nic_tx_offload_off_all() {
    let mut index = 0;
    while let Some(nic) = nic::at(index) {
        if !crate::tasks::service_owns(index) {
            nic.set_tx_offload(false);
        }
        index += 1;
    }
}

pub fn nic_set_multicast(index: usize, on: bool) -> bool {
    match nic::at(index) {
        Some(nic) => {
            nic.set_multicast(on);
            true
        }
        None => false,
    }
}
