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

use core::sync::atomic::{AtomicU64, Ordering};
use kernel_core::nic::{self, MAX_NICS};
use kernel_core::nicctl;

use crate::net::lock_nics;
use crate::{process, sched};

/// The longest frame a tape may send.
pub const MAX_FRAME: u64 = 1518;
const OWNERS_FREE: AtomicU64 = AtomicU64::new(0);
static OWNER: [AtomicU64; MAX_NICS] = [OWNERS_FREE; MAX_NICS];

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
    }
}
