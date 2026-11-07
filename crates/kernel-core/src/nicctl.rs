//! Per-interface receive FIFOs for tapes, and the small NIC control calls the tape ABI forwards (timestamps, multicast, VLAN,
//! offload, counters). They sit on the NIC table in `nic.rs`; the terminal protocol that fills the FIFOs stays in the kernel.

use crate::nic::{self, MAX_NICS};

/// Frames that were not the terminal's, kept per interface for the tape's `nic_receive`
/// (a FIFO twice the hardware ring: a tape that does not poll loses the oldest traffic, like a NIC ring
/// overrun; 64 slots made `nettest` lose small frames long before the NIC did).
const FRAME_FIFO: usize = 256;
/// The FIFO storage. The embedded kernels keep it in .bss. With `vnic` (the server kernel, 16 interfaces) .bss would grow by
/// 6 MiB and the BIOS loader only reserves 8 MiB for a kernel image (`KERNEL_SPAN`, stage2.S), so each interface's FIFO is
/// taken from the kernel's `set_frame_allocator` function on its first frame instead.
#[cfg(not(feature = "vnic"))]
static mut FRAMES: [[u8; 1600]; MAX_NICS * FRAME_FIFO] = [[0; 1600]; MAX_NICS * FRAME_FIFO];
#[cfg(feature = "vnic")]
static mut FRAME_STORE: [*mut u8; MAX_NICS] = [core::ptr::null_mut(); MAX_NICS];
#[cfg(feature = "vnic")]
static mut FRAME_ALLOCATOR: fn(usize) -> *mut u8 = |_| core::ptr::null_mut();

/// Registers the function that returns `bytes` of writable memory (never freed) for an interface's frame FIFO, or null.
#[cfg(feature = "vnic")]
pub fn set_frame_allocator(allocator: fn(usize) -> *mut u8) {
    unsafe { FRAME_ALLOCATOR = allocator };
}

/// Where frame slot `position` of interface `index` lives; null when the FIFO could not be allocated (`create` false: not yet).
#[cfg(not(feature = "vnic"))]
unsafe fn frame_slot(index: usize, position: usize, _create: bool) -> *mut u8 {
    unsafe { (core::ptr::addr_of_mut!(FRAMES) as *mut u8).add((index * FRAME_FIFO + position) * 1600) }
}
#[cfg(feature = "vnic")]
unsafe fn frame_slot(index: usize, position: usize, create: bool) -> *mut u8 {
    unsafe {
        let store = core::ptr::addr_of_mut!(FRAME_STORE) as *mut *mut u8;
        if (*store.add(index)).is_null() && create {
            *store.add(index) = (FRAME_ALLOCATOR)(FRAME_FIFO * 1600);
        }
        let base = *store.add(index);
        if base.is_null() { base } else { base.add(position * 1600) }
    }
}
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
        let storage = frame_slot(index, head, true);
        if storage.is_null() {
            FRAMES_DROPPED[index] += 1;
            return;
        }
        core::slice::from_raw_parts_mut(storage, frame.len()).copy_from_slice(frame);
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
        out[..length].copy_from_slice(core::slice::from_raw_parts(frame_slot(index, FRAME_TAIL[index], false), length));
        LAST_STAMP[index] = FRAME_STAMPS[slot];
        FRAME_TAIL[index] = (FRAME_TAIL[index] + 1) % FRAME_FIFO;
        length
    }
}

/// The hardware receive stamp of the frame `pop_frame` (so a tape's `nic_receive`) returned last on `index`; 0 if none.
pub fn last_rx_stamp(index: usize) -> u64 {
    if index >= MAX_NICS { 0 } else { unsafe { LAST_STAMP[index] } }
}

/// The terminal's (or a server's) own frame handling: true when it consumed the frame, so it is not queued for a tape.
static mut FRAME_HANDLER: fn(usize, &[u8]) -> bool = |_, _| false;

pub fn set_frame_handler(handler: fn(usize, &[u8]) -> bool) {
    unsafe { FRAME_HANDLER = handler };
}

/// Hands one received frame to the registered handler.
pub fn handle_frame(index: usize, frame: &[u8]) -> bool {
    let handler = unsafe { FRAME_HANDLER };
    handler(index, frame)
}

/// Moves every frame waiting in every NIC's hardware ring into the per-interface FIFO (the handler's own frames are
/// dealt with on the way). The part of the tape service a receive-heavy tape needs on every call.
pub fn drain_nics_to_fifo() {
    for index in 0..nic::count() {
        drain_nic_to_fifo(index);
    }
}

/// Where a received frame goes when virtual interfaces share the card.
#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    /// The physical interface (its own MAC, or a unicast address nobody here owns: a bridging tape may want it).
    Parent,
    /// The virtual interface with that MAC.
    Child(usize),
    /// Broadcast and multicast: the parent and every virtual interface.
    Everyone,
}

/// Picks the receiver of `frame` from its destination MAC; `children` are the virtual interfaces (index, MAC).
pub fn classify(frame: &[u8], children: &[(usize, [u8; 6])]) -> Target {
    let Some(destination) = frame.get(..6) else { return Target::Parent };
    if destination[0] & 1 != 0 {
        return Target::Everyone;
    }
    children.iter().find(|(_, mac)| mac[..] == *destination).map_or(Target::Parent, |&(index, _)| Target::Child(index))
}

fn count_virtual_received(child: usize) {
    if let Some(nic::Nic::Virtual(nic)) = nic::at(child) {
        nic.frames_received += 1;
    }
}

/// Moves the frames waiting in interface `index`'s hardware ring into the FIFOs. A virtual interface drains its parent, which
/// splits the traffic by destination MAC (`classify`); a frame the handler consumes (the kernel's own) is never copied.
pub fn drain_nic_to_fifo(index: usize) {
    if let Some(parent) = nic::virtual_parent(index) {
        return drain_nic_to_fifo(parent);
    }
    let mut found = [(0usize, [0u8; 6]); MAX_NICS];
    let count = nic::virtual_children(index, &mut found);
    let children = &found[..count];
    let mut buffer = [0u8; 1600];
    // At most one ring's worth per call: under a line-rate flood the ring never stays empty, and an
    // unbounded loop here would starve the tape that is supposed to consume (and send) the traffic.
    for _ in 0..256 {
        let Some(length) = nic::at(index).and_then(|nic| nic.receive(&mut buffer)) else { break };
        let stamp = nic::at(index).map_or(0, |nic| nic.take_stamp());
        let frame = &buffer[..length];
        let target = classify(frame, children);
        if let Target::Child(child) = target {
            count_virtual_received(child);
            queue_frame(child, frame, stamp);
            continue;
        }
        if handle_frame(index, frame) {
            continue;
        }
        queue_frame(index, frame, stamp);
        if target == Target::Everyone {
            for &(child, _) in children {
                count_virtual_received(child);
                queue_frame(child, frame, 0);
            }
        }
    }
}

/// Moves the frames waiting on the NICs that services own into their queues (a service's own `nic_receive` does this
/// too; the shell's `poll` and a foreground tape's service loop keep the rings from overrunning meanwhile).
pub fn service_poll() {
    for index in 0..nic::count() {
        if crate::tasks::service_owns(index) {
            drain_nic_to_fifo(index);
        }
    }
}

/// A service's `nic_receive`: only on its own NIC.
pub fn service_receive(index: usize, out: &mut [u8]) -> usize {
    if !crate::tasks::current_owns(index) {
        return 0;
    }
    if frame_pending(index) {
        return pop_frame(index, out);
    }
    drain_nic_to_fifo(index);
    pop_frame(index, out)
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
    if !on && nic::has_virtual(index) {
        return true; // the virtual interfaces need it
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    const A: [u8; 6] = [0x02, 0, 0, 0, 0, 1];
    const B: [u8; 6] = [0x02, 0, 0, 0, 0, 2];

    fn frame(destination: [u8; 6]) -> [u8; 60] {
        let mut bytes = [0u8; 60];
        bytes[..6].copy_from_slice(&destination);
        bytes
    }

    #[test]
    fn unicast_goes_to_the_owner_of_the_mac() {
        let children = [(4, A), (5, B)];
        assert_eq!(classify(&frame(A), &children), Target::Child(4));
        assert_eq!(classify(&frame(B), &children), Target::Child(5));
    }

    #[test]
    fn unknown_unicast_stays_with_the_parent() {
        assert_eq!(classify(&frame([0x02, 0, 0, 0, 0, 9]), &[(4, A)]), Target::Parent);
        assert_eq!(classify(&frame(A), &[]), Target::Parent);
    }

    #[test]
    fn broadcast_and_multicast_reach_everyone() {
        assert_eq!(classify(&frame([0xff; 6]), &[(4, A)]), Target::Everyone);
        assert_eq!(classify(&frame([0x01, 0, 0x5e, 0, 0, 0xfb]), &[(4, A)]), Target::Everyone);
    }

    #[test]
    fn a_runt_frame_goes_to_the_parent() {
        assert_eq!(classify(&[0xff; 3], &[(4, A)]), Target::Parent);
    }
}
