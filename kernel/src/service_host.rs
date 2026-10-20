//! What `tasks.rs` (the shared service scheduler) needs from the rest of its kernel: NIC bookkeeping, loading a tape
//! into a region of its own, building its `TapeApi`, and the FP/SIMD units. One small block per kernel.

#[cfg(target_arch = "x86_64")]
mod imp {
    use fosfor_abi::tape::TapeApi;

    use crate::{tape, term};

    pub const API_BYTES: usize = core::mem::size_of::<TapeApi>();

    /// Gives frames that arrive for a service's NIC to the service alone.
    pub fn service_poll() {
        term::service_poll();
    }

    pub fn nic_count() -> usize {
        term::nic_count()
    }

    /// A NIC has just become a service's: drop frames queued for the foreground.
    pub fn nic_claim(nic: usize) {
        term::clear_frame_queue_nic(nic);
    }

    /// The service ended: undo the multicast, VLAN, offload and stamp state it left on the card.
    pub fn nic_release(nic: usize) {
        term::nic_release(nic);
    }

    /// Reads nothing itself: `run_named` already staged the file; relocates it into region `slot`.
    pub fn load(slot: usize, name: &str) -> Option<u64> {
        match tape::load_staged_service(slot, name) {
            Ok(entry) => Some(entry),
            Err(error) => {
                crate::println!("run: {name} is not a usable tape ({error:?}).");
                None
            }
        }
    }

    pub unsafe fn write_api(at: u64, arguments: &'static str) {
        let Some(info) = tape::boot_info() else { return };
        unsafe { (at as *mut TapeApi).write(tape::build_api(info, arguments)) };
    }

    /// A service may be a hard-float tape: the units stay on while any service runs.
    pub fn services_begin() {
        tape::enable_fp_level();
    }

    pub fn services_over() {
        if !term::tape_mode() {
            tape::clear_fp_level();
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod imp {
    use fosfor_abi::tape::TapeApi;

    use crate::tape_aarch64 as tape;

    pub const API_BYTES: usize = core::mem::size_of::<TapeApi>();

    pub fn service_poll() {
        crate::usb_kbd::poll();
    }

    pub fn nic_count() -> usize {
        usize::from(crate::genet::present())
    }

    /// The Genet has no queue between the card and the tape: ownership is enforced in `api_nic_receive`.
    pub fn nic_claim(_nic: usize) {}

    pub fn nic_release(_nic: usize) {}

    pub fn load(slot: usize, name: &str) -> Option<u64> {
        // Regions 0 and 1 belong to `run A + B`; services follow.
        match tape::load_slot(name, slot + 1) {
            Ok(entry) => Some(entry),
            Err(reason) => {
                crate::println!("run: {name}: {reason}.");
                None
            }
        }
    }

    pub unsafe fn write_api(at: u64, arguments: &'static str) {
        unsafe { tape::write_api(at, arguments) };
    }

    /// The Pi enables the FP/SIMD units for the first tape and never turns them off again.
    pub fn services_begin() {
        crate::cpu::fp_enable();
    }

    pub fn services_over() {}
}

pub use imp::*;
