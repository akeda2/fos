//! Network calls for tapes (`nic_*`), on the shared `kernel_core::nicctl`: the per-interface receive FIFOs, the hardware
//! controls (promiscuous, multicast groups, VLAN filter, transmit offload, timestamps, counters) and the counters.
//!
//! The NIC table and the FIFOs are statics that know nothing of other CPUs, so every call runs under `net::lock_nics` (the service
//! thread's polling takes the same lock). A tape's memory is never handed to a driver: frames go through kernel buffers.
//!
//! One process drives an interface at a time: the first process to send, receive or change a setting on a NIC owns it until it
//! ends (`nic_owned` says so); the kernel's own terminal, discovery and KUP frames (ethertype 0x88b6) never reach a tape. When the
//! owner ends, the interface goes back to the state a tape leaves it in (no promiscuous or multicast mode, no VLAN filter or
//! offload, timestamps for PTP events only) and its queued frames are dropped.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use kernel_core::nic::{self, MAX_NICS};
use kernel_core::nicctl;

use crate::net::lock_nics;
use crate::{process, sched};

/// The longest frame a tape may send.
pub const MAX_FRAME: u64 = 1518;
const OWNERS_FREE: AtomicU64 = AtomicU64::new(0);
static OWNER: [AtomicU64; MAX_NICS] = [OWNERS_FREE; MAX_NICS];

const NO_ADDRESS: AtomicU32 = AtomicU32::new(0);
/// The IPv4 address the tape on each interface sends from (network byte order as a big-endian number), 0 while unknown. The
/// kernel does not run IP: a tape does its own DHCP, so the address is read off the frames it sends (an IPv4 source address
/// or the sender of an ARP packet) to let `nic` and `service` say which address a service got.
static SEEN_ADDRESS: [AtomicU32; MAX_NICS] = [NO_ADDRESS; MAX_NICS];

fn note_address(index: usize, frame: &[u8]) {
    let ethertype = frame.get(12..14).map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]));
    let source = match ethertype {
        Some(0x0800) => frame.get(26..30),
        Some(0x0806) => frame.get(28..32),
        _ => None,
    };
    let Some(source) = source else { return };
    let address = u32::from_be_bytes([source[0], source[1], source[2], source[3]]);
    // 0.0.0.0 is a DHCP client or an ARP probe that has no address yet.
    if address != 0 && SEEN_ADDRESS[index].load(Ordering::Relaxed) != address {
        SEEN_ADDRESS[index].store(address, Ordering::Relaxed);
    }
}

/// A quiet service (an idle sshd) sends nothing after its DHCP lease, so a unicast frame that arrives also tells: it was
/// addressed to this interface's MAC (broadcasts and multicasts say nothing about it), so its IPv4 destination or the target
/// of a unicast ARP is the interface's address.
fn note_received_address(index: usize, frame: &[u8]) {
    if frame.first().is_none_or(|byte| byte & 1 != 0) {
        return;
    }
    let target = match frame.get(12..14) {
        Some([0x08, 0x00]) => frame.get(30..34),
        Some([0x08, 0x06]) => frame.get(38..42),
        _ => None,
    };
    let Some(target) = target else { return };
    let address = u32::from_be_bytes([target[0], target[1], target[2], target[3]]);
    if address != 0 && address != u32::MAX && target[0] < 224 && SEEN_ADDRESS[index].load(Ordering::Relaxed) != address {
        SEEN_ADDRESS[index].store(address, Ordering::Relaxed);
    }
}

/// The address a tape was seen to send from on interface `index`.
pub fn address(index: usize) -> Option<[u8; 4]> {
    let address = SEEN_ADDRESS.get(index)?.load(Ordering::Relaxed);
    (address != 0).then(|| address.to_be_bytes())
}

/// The address of the interface the process `pid` owns, if it has sent from one.
pub fn address_of_owner(pid: u64) -> Option<[u8; 4]> {
    if pid == 0 {
        return None;
    }
    (0..MAX_NICS).filter(|&index| OWNER[index].load(Ordering::Acquire) == pid).find_map(address)
}

/// Makes the calling process the owner of NIC `index` (or confirms it); false if it does not exist or another process owns it.
fn claim(index: u64) -> bool {
    let index = index as usize;
    if index >= nic::count().min(MAX_NICS) {
        return false;
    }
    let pid = sched::current_pid();
    match OWNER[index].compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => {
            // Traffic that arrived before this tape started is stale to it.
            let _nics = lock_nics();
            nicctl::clear_frame_queue_nic(index);
            true
        }
        Err(owner) => owner == pid,
    }
}

/// Whether NIC `index` (0-based) exists.
pub fn exists(index: usize) -> bool {
    index < nic::count().min(MAX_NICS)
}

/// The pid that owns NIC `index`, 0 when nobody does.
pub fn owner(index: usize) -> u64 {
    OWNER[index].load(Ordering::Acquire)
}

/// A background tape (`run NAME &N`) owns the NICs in `mask` from the start; `active` then answers with the first of them.
pub fn reserve(pid: u64, mask: u64) {
    for index in (0..MAX_NICS).filter(|&index| mask >> index & 1 != 0) {
        if OWNER[index].compare_exchange(0, pid, Ordering::AcqRel, Ordering::Acquire).is_ok() {
            let _nics = lock_nics();
            nicctl::clear_frame_queue_nic(index);
        }
    }
}

/// `nic_active`: the lowest NIC this process owns (the one a background tape was started on), else 0.
pub fn active() -> u64 {
    let pid = sched::current_pid();
    (0..MAX_NICS).find(|&index| OWNER[index].load(Ordering::Acquire) == pid).unwrap_or(0) as u64
}

/// `nic_owned(index)`: 1 when this process may drive the interface.
pub fn owned(index: u64) -> u64 {
    let free_or_mine = (index as usize) < nic::count().min(MAX_NICS) && {
        let owner = OWNER[index as usize].load(Ordering::Acquire);
        owner == 0 || owner == sched::current_pid()
    };
    u64::from(free_or_mine)
}

pub fn count() -> u64 {
    nic::count() as u64
}

/// `nic_mac(index, out)`.
pub fn mac(index: u64, out: u64) -> u64 {
    let mut address = [0u8; 6];
    let found = {
        let _nics = lock_nics();
        nicctl::nic_mac(index as usize, &mut address)
    };
    u64::from(found && process::copy_to_user(out, &address))
}

pub fn send(index: u64, frame: u64, length: u64) -> u64 {
    if length > MAX_FRAME || !claim(index) {
        return 0;
    }
    let Some(bytes) = process::user_bytes_up_to(frame, length, MAX_FRAME) else { return 0 };
    note_address(index as usize, &bytes);
    let _nics = lock_nics();
    u64::from(nicctl::nic_send(index as usize, &bytes))
}

/// `nic_receive(index, buffer, capacity)`: the length of the next frame (0 for none), truncated to `capacity`.
pub fn receive(index: u64, buffer: u64, capacity: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let mut frame = [0u8; 1600];
    let length = {
        let _nics = lock_nics();
        let index = index as usize;
        if !nicctl::frame_pending(index) {
            nicctl::drain_nic_to_fifo(index);
        }
        nicctl::pop_frame(index, &mut frame)
    };
    let length = length.min(capacity as usize);
    if length > 0 {
        note_received_address(index as usize, &frame[..length]);
    }
    if length > 0 && !process::copy_to_user(buffer, &frame[..length]) {
        return 0;
    }
    length as u64
}

/// The on/off settings that take the interface's owner only: `change(index, on)`.
pub fn setting(index: u64, on: u64, change: fn(usize, bool) -> bool) -> u64 {
    if !claim(index) {
        return 0;
    }
    let _nics = lock_nics();
    u64::from(change(index as usize, on != 0))
}

pub fn multicast_join(index: u64, mac: u64, on: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let Some(bytes) = process::user_bytes_up_to(mac, 6, 6) else { return 0 };
    let _nics = lock_nics();
    u64::from(nicctl::nic_multicast_join(index as usize, bytes.try_into().unwrap(), on != 0))
}

pub fn vlan_filter(index: u64, vid: u64, on: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let Ok(vid) = u16::try_from(vid) else { return 0 };
    let _nics = lock_nics();
    u64::from(nicctl::nic_vlan_filter(index as usize, vid, on != 0))
}

/// `nic_counters(index, out, count)`: up to 9 u64 values.
pub fn counters(index: u64, out: u64, count: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let mut values = [0u64; 9];
    let wrote = {
        let _nics = lock_nics();
        nicctl::nic_counters(index as usize, &mut values[..(count as usize).min(9)])
    };
    let bytes: alloc::vec::Vec<u8> = values[..wrote].iter().flat_map(|value| value.to_le_bytes()).collect();
    if process::copy_to_user(out, &bytes) { wrote as u64 } else { 0 }
}

pub fn ts_clock(index: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let _nics = lock_nics();
    nicctl::nic_ts_clock(index as usize)
}

pub fn ts_adjust(index: u64, ppb: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let _nics = lock_nics();
    u64::from(nicctl::nic_ts_adjust(index as usize, ppb as i32))
}

pub fn ts_step(index: u64, delta: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    let _nics = lock_nics();
    u64::from(nicctl::nic_ts_step(index as usize, delta as i64))
}

pub fn ts_rx(index: u64) -> u64 {
    if !claim(index) {
        return 0;
    }
    nicctl::last_rx_stamp(index as usize)
}

/// `nic_send_stamped(index, frame, length, stamp_out)`.
pub fn send_stamped(index: u64, frame: u64, length: u64, stamp_out: u64) -> u64 {
    if length > MAX_FRAME || !claim(index) {
        return 0;
    }
    let Some(bytes) = process::user_bytes_up_to(frame, length, MAX_FRAME) else { return 0 };
    note_address(index as usize, &bytes);
    let (sent, stamp) = {
        let _nics = lock_nics();
        nicctl::nic_send_stamped(index as usize, &bytes)
    };
    if stamp_out != 0 {
        process::copy_to_user(stamp_out, &stamp.to_le_bytes());
    }
    u64::from(sent)
}

/// The process `pid` ended: every interface it owned goes back to the state a tape leaves it in.
pub fn process_ended(pid: u64) {
    for index in 0..MAX_NICS {
        if OWNER[index].compare_exchange(pid, 0, Ordering::AcqRel, Ordering::Acquire).is_err() {
            continue;
        }
        let _nics = lock_nics();
        nicctl::nic_release(index);
        nicctl::nic_set_promiscuous(index, false);
        nicctl::nic_set_multicast(index, false);
        nicctl::clear_frame_queue_nic(index);
        crate::vnic::tape_ended(index);
        SEEN_ADDRESS[index].store(0, Ordering::Relaxed);
    }
}
