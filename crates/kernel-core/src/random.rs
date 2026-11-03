//! The random bytes behind tape ABI 33 (`random`): one pool shared by every caller, in the kernel's own memory.
//!
//! Each request hashes (SHA-256) the pool, a counter, the cycle counter and, where the CPU has them, eight bytes from
//! RDSEED/RDRAND, then keeps a second hash as the next pool, so one output reveals nothing about the next and a CPU
//! without a hardware generator still gets a pool that differs at every call and every boot (the timer is read at
//! nanosecond scale, whose low bits a remote peer cannot predict). That is *not* a hardware entropy source: `fill`
//! returns `true` only when RDSEED or RDRAND really contributed, and a tape that needs real secrets should refuse a
//! `false`. aarch64 (the Pi 4's Cortex-A72 has no RNDR) reports `false` always.
use core::sync::atomic::{AtomicBool, Ordering};
use tape_sync::sha256;

static LOCK: AtomicBool = AtomicBool::new(false);
static mut POOL: [u8; 32] = [0; 32];
static mut COUNTER: u64 = 0;

#[cfg(target_arch = "x86_64")]
fn cycles() -> u64 {
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(target_arch = "aarch64")]
fn cycles() -> u64 {
    let value: u64;
    unsafe { core::arch::asm!("mrs {}, cntvct_el0", out(reg) value) };
    value
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
fn cycles() -> u64 {
    0
}

/// Eight bytes from the CPU's generator, if it has one and answers within a few tries.
#[cfg(target_arch = "x86_64")]
fn hardware() -> Option<u64> {
    use core::arch::x86_64::{__cpuid, __cpuid_count, _rdrand64_step, _rdseed64_step};
    let mut value = 0u64;
    unsafe {
        if __cpuid_count(7, 0).ebx & (1 << 18) != 0 && (0..16).any(|_| _rdseed64_step(&mut value) == 1) {
            return Some(value);
        }
        if __cpuid(1).ecx & (1 << 30) != 0 && (0..16).any(|_| _rdrand64_step(&mut value) == 1) {
            return Some(value);
        }
    }
    None
}

#[cfg(not(target_arch = "x86_64"))]
fn hardware() -> Option<u64> {
    None
}

/// Fills `out`; the answer says whether a hardware generator contributed to every block.
pub fn fill(out: &mut [u8]) -> bool {
    while LOCK.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
        core::hint::spin_loop();
    }
    let mut strong = true;
    #[allow(static_mut_refs)]
    let (pool, counter) = unsafe { (&mut *core::ptr::addr_of_mut!(POOL), &mut *core::ptr::addr_of_mut!(COUNTER)) };
    for chunk in out.chunks_mut(32) {
        *counter = counter.wrapping_add(1);
        let hard = hardware();
        strong &= hard.is_some();
        let mut input = [0u8; 32 + 8 + 8 + 8 + 1];
        input[..32].copy_from_slice(pool);
        input[32..40].copy_from_slice(&counter.to_le_bytes());
        input[40..48].copy_from_slice(&cycles().to_le_bytes());
        input[48..56].copy_from_slice(&hard.unwrap_or(0).to_le_bytes());
        let block = sha256(&input);
        input[56] = 1;
        let mut next = [0u8; 57 + 32];
        next[..57].copy_from_slice(&input);
        next[57..].copy_from_slice(&block);
        *pool = sha256(&next);
        chunk.copy_from_slice(&block[..chunk.len()]);
    }
    LOCK.store(false, Ordering::Release);
    strong
}

/// Tape ABI 33 `random(buffer, length)`: fills the buffer; 1 when a hardware generator contributed, 0 when not.
pub extern "C" fn api_random(buffer: *mut u8, length: usize) -> isize {
    let out = unsafe { core::slice::from_raw_parts_mut(buffer, length) };
    isize::from(fill(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_differ_and_long_requests_work() {
        let (mut a, mut b) = ([0u8; 100], [0u8; 100]);
        fill(&mut a);
        fill(&mut b);
        assert_ne!(a, b);
        assert_ne!(a[..32], a[32..64]);
        assert!(a.iter().any(|&x| x != 0));
    }
}
