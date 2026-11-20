//! What the Recorder's drawing thread and its worker (the thread that encodes and writes a take) share: a ring of captured
//! bytes and two mailboxes. No locks: each has one writer and one reader, and on the server kernel the two run on
//! different CPUs at once.
//!
//! - `Ring`: the drawing thread pushes whole blocks of device frames, the worker takes them. The capacity is a whole
//!   number of frames, so a contiguous run the worker sees is whole frames too. A push that does not fit is refused
//!   whole; the drawing thread then leaves the audio in the kernel's capture ring (far larger) instead of dropping it.
//! - `Mailbox<T>`: a value with a serial number, written by one side, read by the other; a reader sees each value whole
//!   (a sequence lock), and knows from the serial whether it is new. Commands go one way, the take's status the other.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering, fence};

pub struct Ring {
    buffer: AtomicPtr<u8>,
    /// The memory given (`attach`), and the part of it in use: a whole number of frames.
    length: AtomicUsize,
    capacity: AtomicUsize,
    /// Bytes ever pushed and ever taken: their difference is what waits, `% capacity` where.
    pushed: AtomicU64,
    taken: AtomicU64,
}

impl Ring {
    pub const fn new() -> Ring {
        Ring {
            buffer: AtomicPtr::new(core::ptr::null_mut()),
            length: AtomicUsize::new(0),
            capacity: AtomicUsize::new(0),
            pushed: AtomicU64::new(0),
            taken: AtomicU64::new(0),
        }
    }

    /// Gives the ring its memory: the largest whole number of `frame`-byte frames that fits `memory`. Call it before the
    /// worker starts (or while no take runs and the ring is empty).
    pub fn attach(&self, memory: &'static mut [u8], frame: usize) {
        self.buffer.store(memory.as_mut_ptr(), Ordering::Relaxed);
        self.length.store(memory.len(), Ordering::Relaxed);
        self.reframe(frame);
    }

    /// Another frame size for the same memory (the device's channels change with its rate). Empties the ring; only while
    /// no take runs, so nothing is pushed or taken meanwhile.
    pub fn reframe(&self, frame: usize) {
        let capacity = self.length.load(Ordering::Relaxed) / frame.max(1) * frame.max(1);
        self.capacity.store(capacity, Ordering::Relaxed);
        // From zero, not from where the counters were: the old count is whole frames of the old size, and taken modulo
        // the new capacity would start the ring mid-frame.
        self.taken.store(0, Ordering::Relaxed);
        self.pushed.store(0, Ordering::Release);
    }

    pub fn capacity(&self) -> usize {
        self.capacity.load(Ordering::Relaxed)
    }

    /// Bytes waiting for the worker.
    pub fn waiting(&self) -> usize {
        // Saturating: a reader that overlaps `reframe` may see one counter zeroed and not the other.
        self.pushed.load(Ordering::Acquire).saturating_sub(self.taken.load(Ordering::Acquire)) as usize
    }

    /// Room for a push of this size (drawing thread).
    pub fn has_room(&self, bytes: usize) -> bool {
        self.capacity() - self.waiting() >= bytes
    }

    /// Copies `data` in whole, or nothing (drawing thread only). False when it does not fit.
    pub fn push(&self, data: &[u8]) -> bool {
        let capacity = self.capacity();
        if capacity == 0 || !self.has_room(data.len()) {
            return false;
        }
        let pushed = self.pushed.load(Ordering::Relaxed);
        let at = (pushed % capacity as u64) as usize;
        let first = data.len().min(capacity - at);
        let buffer = self.buffer.load(Ordering::Relaxed);
        // SAFETY: the bytes from `pushed` on, `data.len()` of them (wrapping), are free: the worker reads only below
        // `pushed`, which moves past them only after the copy (Release below).
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), buffer.add(at), first);
            core::ptr::copy_nonoverlapping(data.as_ptr().add(first), buffer, data.len() - first);
        }
        self.pushed.store(pushed + data.len() as u64, Ordering::Release);
        true
    }

    /// The bytes waiting that lie in one piece, oldest first (worker only); give back what was used with `take`.
    pub fn peek(&self) -> &[u8] {
        let capacity = self.capacity();
        if capacity == 0 {
            return &[];
        }
        let taken = self.taken.load(Ordering::Relaxed);
        let waiting = self.pushed.load(Ordering::Acquire).saturating_sub(taken) as usize;
        let at = (taken % capacity as u64) as usize;
        let length = waiting.min(capacity - at);
        // SAFETY: these bytes were published by the drawing thread's Release store of `pushed`, and it does not touch them
        // again until `take` moves `taken` past them.
        unsafe { core::slice::from_raw_parts(self.buffer.load(Ordering::Relaxed).add(at), length) }
    }

    /// Gives back the first `bytes` of what `peek` showed (worker only).
    pub fn take(&self, bytes: usize) {
        self.taken.fetch_add(bytes as u64, Ordering::Release);
    }

    /// Drops everything waiting (worker only: a take that could not start).
    pub fn clear(&self) {
        self.taken.store(self.pushed.load(Ordering::Acquire), Ordering::Release);
    }
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: one thread pushes and one takes, and the counters publish the bytes between them (above).
unsafe impl Sync for Ring {}

/// A value one side posts and the other reads whole, with a serial that grows on every post.
pub struct Mailbox<T: Copy> {
    /// Even when the value is stable; odd while it is being written. Twice the number of posts.
    sequence: AtomicU32,
    value: UnsafeCell<T>,
}

impl<T: Copy> Mailbox<T> {
    pub const fn new(value: T) -> Mailbox<T> {
        Mailbox { sequence: AtomicU32::new(0), value: UnsafeCell::new(value) }
    }

    /// Replaces the value (one writer only).
    pub fn post(&self, value: T) {
        let sequence = self.sequence.load(Ordering::Relaxed);
        self.sequence.store(sequence.wrapping_add(1), Ordering::Relaxed);
        fence(Ordering::Release);
        // SAFETY: only this side writes; a reader that overlaps sees an odd or changed sequence and reads again.
        unsafe { core::ptr::write_volatile(self.value.get(), value) };
        self.sequence.store(sequence.wrapping_add(2), Ordering::Release);
    }

    /// The value and its serial (how many posts it is from the start).
    pub fn read(&self) -> (T, u32) {
        loop {
            let before = self.sequence.load(Ordering::Acquire);
            if before % 2 == 1 {
                core::hint::spin_loop();
                continue;
            }
            // SAFETY: a copy that raced with `post` is thrown away below (the sequence moved); `T` is plain data.
            let value = unsafe { core::ptr::read_volatile(self.value.get()) };
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == before {
                return (value, before / 2);
            }
        }
    }

    pub fn serial(&self) -> u32 {
        self.sequence.load(Ordering::Acquire) / 2
    }
}

// SAFETY: one writer, readers check the sequence (above); `T` is `Copy` plain data.
unsafe impl<T: Copy + Send> Sync for Mailbox<T> {}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::boxed::Box;
    use std::vec;
    use std::vec::Vec;

    fn ring(bytes: usize, frame: usize) -> &'static Ring {
        let ring: &'static Ring = Box::leak(Box::new(Ring::new()));
        ring.attach(Box::leak(vec![0u8; bytes].into_boxed_slice()), frame);
        ring
    }

    #[test]
    fn a_ring_keeps_whole_frames_and_refuses_what_does_not_fit() {
        // 100 bytes of 12-byte frames: 96 usable.
        let r = ring(100, 12);
        assert_eq!(r.capacity(), 96);
        assert!(r.push(&[1; 60]));
        assert!(!r.push(&[2; 48]), "48 more do not fit in 36 free");
        assert_eq!(r.waiting(), 60);
        r.take(48);
        assert!(r.push(&[3; 72]), "it wraps");
        // What waits: 12 bytes of 1, then 72 of 3 (in two pieces across the wrap).
        let mut got = Vec::new();
        while r.waiting() > 0 {
            let piece = r.peek().to_vec();
            assert_eq!(piece.len() % 12, 0, "a contiguous run is whole frames");
            r.take(piece.len());
            got.extend(piece);
        }
        assert_eq!(got.len(), 84);
        assert!(got[..12].iter().all(|&b| b == 1) && got[12..].iter().all(|&b| b == 3));
        assert!(r.push(&[4; 96]));
        r.clear();
        assert_eq!(r.waiting(), 0);
        // The device's frame shrinks (fewer channels at a higher rate): 100 bytes of 8-byte frames.
        r.push(&[5; 24]);
        r.reframe(8);
        assert_eq!((r.capacity(), r.waiting()), (96, 0), "reframing empties the ring");
        r.reframe(36);
        assert_eq!(r.capacity(), 72);
        assert!(r.push(&[6; 72]));
        assert_eq!(r.waiting(), 72);
        assert_eq!(r.peek().len() % 36, 0, "pieces are whole frames of the new size");
    }

    #[test]
    fn two_threads_pass_every_byte_in_order() {
        let r = ring(4096 * 3, 12);
        const BLOCKS: u64 = 20_000;
        let producer = std::thread::spawn(move || {
            let mut block = [0u8; 120];
            for n in 0..BLOCKS {
                for (i, byte) in block.iter_mut().enumerate() {
                    *byte = (n as usize * 7 + i) as u8;
                }
                while !r.push(&block) {
                    std::thread::yield_now();
                }
            }
        });
        let mut seen = 0u64;
        let mut expected = Vec::new();
        while seen < BLOCKS * 120 {
            let piece = r.peek();
            if piece.is_empty() {
                std::thread::yield_now();
                continue;
            }
            for &byte in piece {
                let (n, i) = (seen / 120, seen % 120);
                expected.push(((n as usize * 7 + i as usize) as u8, byte));
                seen += 1;
            }
            let length = piece.len();
            r.take(length);
        }
        producer.join().unwrap();
        assert!(expected.iter().all(|(want, got)| want == got), "every byte arrives, in order");
    }

    #[test]
    fn a_mailbox_is_read_whole_while_it_changes() {
        let mailbox: &'static Mailbox<[u64; 32]> = Box::leak(Box::new(Mailbox::new([0; 32])));
        let writer = std::thread::spawn(move || {
            for n in 1..=50_000u64 {
                mailbox.post([n; 32]);
            }
        });
        let mut last = 0;
        loop {
            let (value, serial) = mailbox.read();
            assert!(value.iter().all(|&v| v == value[0]), "never torn: {:?}", &value[..4]);
            assert!(value[0] >= last, "never older");
            assert_eq!(serial as u64, value[0], "the serial counts the posts");
            last = value[0];
            if last == 50_000 {
                break;
            }
        }
        writer.join().unwrap();
    }
}
