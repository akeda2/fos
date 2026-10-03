//! The test engine behind the `memtest` tape: pure functions over a region of RAM.
//! (Ported from v1: the same tests, widened to 64-bit physical addresses; the 32-bit
//! pools and the long-mode excursion are gone because a 64-bit tape just addresses all
//! of RAM.)
//!
//! RAM faults come in a few flavours, and each test here is aimed at one:
//!
//! * **Address test** -- every word holds (the low 32 bits of) its own physical address, so
//!   a stuck or shorted address line shows up as a word holding someone else's address.
//!   Run once plain and once inverted.
//! * **Walking ones / zeros** -- one bit set (or clear) in an otherwise empty word, for
//!   each of the 32 bit positions: finds stuck data lines.
//! * **Moving inversions** -- fill with a pattern, sweep up verifying and writing the
//!   complement, sweep down doing the reverse: catches coupling faults a plain
//!   fill-and-check never would.
//! * **Random** -- a seeded pseudo-random fill and its complement.
//!
//! Every access is volatile: the compiler must not be allowed to notice that it just
//! wrote a value and "know" what a read will return. Errors are counted, never fatal.

use core::ptr::{read_volatile, write_volatile};

/// Words between two [`Control::poll`] calls: 64 K words = 256 KiB, a few
/// hundred microseconds of work, so abort keys and the watchdog stay lively.
pub const BLOCK_WORDS: usize = 64 * 1024;
/// Mismatches kept in full for reporting; further ones are only counted.
pub const MAX_RECORDED: usize = 16;
/// Distinct 1 MiB blocks remembered in the bad-block table.
pub const MAX_BAD_BLOCKS: usize = 16;
/// Which test found a mismatch.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Test {
    Address,
    Walking,
    Moving,
    Random,
    Retention,
    /// Allocator churn (soak mode): a block filled right after `alloc` and
    /// checked just before `free`, which finds two allocations overlapping.
    Churn,
}

impl Test {
    pub fn name(self) -> &'static [u8] {
        match self {
            Test::Address => b"addr",
            Test::Walking => b"walk",
            Test::Moving => b"moving",
            Test::Random => b"random",
            Test::Retention => b"hold",
            Test::Churn => b"churn",
        }
    }
}

/// One wrong word.
#[derive(Clone, Copy, Debug)]
pub struct Mismatch {
    pub address: u64,
    pub expected: u32,
    pub actual: u32,
    pub test: Test,
}

/// Everything the tests found, accumulated across regions and passes.
pub struct Errors {
    pub total: u32,
    pub recorded: [Mismatch; MAX_RECORDED],
    pub recorded_count: usize,
    /// How often each bit position was wrong (a stuck bit piles up in one slot).
    pub bit_flips: [u32; 32],
    /// `(address >> 20, errors)` for each 1 MiB block with at least one error.
    pub bad_blocks: [(u64, u32); MAX_BAD_BLOCKS],
    pub bad_block_count: usize,
    /// Bytes read back and compared, over the whole run.
    pub bytes_checked: u64,
}

impl Errors {
    pub const fn new() -> Self {
        Errors {
            total: 0,
            recorded: [Mismatch { address: 0, expected: 0, actual: 0, test: Test::Address }; MAX_RECORDED],
            recorded_count: 0,
            bit_flips: [0; 32],
            bad_blocks: [(0, 0); MAX_BAD_BLOCKS],
            bad_block_count: 0,
            bytes_checked: 0,
        }
    }

    fn record(&mut self, address: u64, expected: u32, actual: u32, test: Test) {
        self.total = self.total.saturating_add(1);
        let difference = expected ^ actual;
        for bit in 0..32 {
            if difference >> bit & 1 != 0 {
                self.bit_flips[bit] = self.bit_flips[bit].saturating_add(1);
            }
        }
        let block = address >> 20;
        match self.bad_blocks[..self.bad_block_count].iter_mut().find(|entry| entry.0 == block) {
            Some(entry) => entry.1 = entry.1.saturating_add(1),
            None if self.bad_block_count < MAX_BAD_BLOCKS => {
                self.bad_blocks[self.bad_block_count] = (block, 1);
                self.bad_block_count += 1;
            }
            None => {}
        }
        // One line per bad address: a stuck cell fails every test, and the repeats would
        // push the other bad addresses off the list.
        let known = self.recorded[..self.recorded_count].iter().any(|seen| seen.address == address);
        if self.recorded_count < MAX_RECORDED && !known {
            self.recorded[self.recorded_count] = Mismatch { address, expected, actual, test };
            self.recorded_count += 1;
        }
    }
}

/// How the engine talks to its caller while it works.
pub trait Control {
    /// Called every [`BLOCK_WORDS`] words. Return `false` to stop the test now.
    fn poll(&mut self) -> bool;
    /// Called after each write sweep, before the sweep that verifies it. The
    /// tape's `inject=` option uses this to flip a bit on purpose, which is how
    /// the end-to-end test proves errors really are detected (QEMU's RAM never
    /// fails by itself). The default does nothing.
    fn after_write(&mut self, _region: &Region) {}
}


/// A block of physical RAM the tape owns.
#[derive(Clone, Copy)]
pub struct Region {
    ptr: *mut u32,
    words: usize,
    base: u64,
}

impl Region {
    /// # Safety
    /// `ptr` must be valid for reads and writes of `words` 32-bit words and not used by
    /// anything else while the region is tested. `base` is the physical address of the
    /// first word (the address test stores its low 32 bits).
    pub unsafe fn new(ptr: *mut u32, words: usize, base: u64) -> Self {
        Region { ptr, words, base }
    }

    pub fn words(&self) -> usize {
        self.words
    }

    pub fn base(&self) -> u64 {
        self.base
    }

    fn physical(&self, index: usize) -> u64 {
        self.base + index as u64 * 4
    }

    fn address(&self, index: usize) -> u32 {
        self.physical(index) as u32
    }

    fn read(&self, index: usize) -> u32 {
        unsafe { read_volatile(self.ptr.add(index)) }
    }

    fn write(&self, index: usize, value: u32) {
        unsafe { write_volatile(self.ptr.add(index), value) }
    }

    /// XORs `mask` into one word (fault injection).
    pub fn corrupt(&self, index: usize, mask: u32) {
        if index < self.words {
            self.write(index, self.read(index) ^ mask);
        }
    }
}

/// xorshift32: fast, never zero, repeatable from its seed.
fn next_random(state: &mut u32) -> u32 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *state = x;
    x
}

/// A seed xorshift32 can use (it must not be zero).
fn usable_seed(seed: u32) -> u32 {
    if seed == 0 { 0x9e37_79b9 } else { seed }
}

/// Writes `value_of(index)` to every word. `false` means aborted.
fn fill(region: &Region, control: &mut dyn Control, mut value_of: impl FnMut(usize) -> u32) -> bool {
    let mut index = 0;
    while index < region.words {
        let end = (index + BLOCK_WORDS).min(region.words);
        for i in index..end {
            region.write(i, value_of(i));
        }
        index = end;
        if !control.poll() {
            return false;
        }
    }
    control.after_write(region);
    true
}

/// Reads every word and compares it with `value_of(index)`. `false` = aborted.
fn verify(
    region: &Region,
    errors: &mut Errors,
    control: &mut dyn Control,
    test: Test,
    mut value_of: impl FnMut(usize) -> u32,
) -> bool {
    let mut index = 0;
    while index < region.words {
        let end = (index + BLOCK_WORDS).min(region.words);
        for i in index..end {
            let expected = value_of(i);
            let actual = region.read(i);
            if actual != expected {
                errors.record(region.physical(i), expected, actual, test);
            }
        }
        errors.bytes_checked += (end - index) as u64 * 4;
        index = end;
        if !control.poll() {
            return false;
        }
    }
    true
}

/// Fills the region with one repeated word (the first half of a hold test, and
/// the building block of the walking tests).
pub fn fill_pattern(region: &Region, control: &mut dyn Control, pattern: u32) -> bool {
    fill(region, control, |_| pattern)
}

/// Checks the region still holds `pattern` everywhere.
pub fn verify_pattern(
    region: &Region,
    errors: &mut Errors,
    control: &mut dyn Control,
    test: Test,
    pattern: u32,
) -> bool {
    verify(region, errors, control, test, |_| pattern)
}

/// Each word holds its own address, then its complement. Returns `false` if
/// aborted.
pub fn address_test(region: &Region, errors: &mut Errors, control: &mut dyn Control) -> bool {
    for invert in [0, u32::MAX] {
        let value = |i: usize| region.address(i) ^ invert;
        if !fill(region, control, value) || !verify(region, errors, control, Test::Address, value) {
            return false;
        }
    }
    true
}

/// Walking ones, then walking zeros, across all 32 bit positions.
pub fn walking_test(region: &Region, errors: &mut Errors, control: &mut dyn Control) -> bool {
    for bit in 0..32 {
        for pattern in [1u32 << bit, !(1u32 << bit)] {
            if !fill_pattern(region, control, pattern)
                || !verify_pattern(region, errors, control, Test::Walking, pattern)
            {
                return false;
            }
        }
    }
    true
}

/// Patterns for the moving-inversions test. Together with their complements
/// they put every bit through 0->1 and 1->0 with different neighbours.
pub const MOVING_PATTERNS: [u32; 6] =
    [0x0000_0000, 0x5555_5555, 0x3333_3333, 0x0f0f_0f0f, 0x00ff_00ff, 0x0000_ffff];

/// Moving inversions: fill `p`; up: check `p`, write `!p`; down: check `!p`,
/// write `p`; finally check `p` everywhere.
pub fn moving_test(region: &Region, errors: &mut Errors, control: &mut dyn Control) -> bool {
    for pattern in MOVING_PATTERNS {
        if !fill_pattern(region, control, pattern) {
            return false;
        }
        for (expected, replacement, ascending) in [(pattern, !pattern, true), (!pattern, pattern, false)] {
            if !sweep_invert(region, errors, control, expected, replacement, ascending) {
                return false;
            }
        }
        if !verify_pattern(region, errors, control, Test::Moving, pattern) {
            return false;
        }
    }
    true
}

/// One moving-inversions sweep: for each word (ascending or descending), check
/// it holds `expected` and write `replacement`.
fn sweep_invert(
    region: &Region,
    errors: &mut Errors,
    control: &mut dyn Control,
    expected: u32,
    replacement: u32,
    ascending: bool,
) -> bool {
    let mut done = 0;
    while done < region.words {
        let count = BLOCK_WORDS.min(region.words - done);
        for step in 0..count {
            let i = if ascending { done + step } else { region.words - 1 - done - step };
            let actual = region.read(i);
            if actual != expected {
                errors.record(region.physical(i), expected, actual, Test::Moving);
            }
            region.write(i, replacement);
        }
        errors.bytes_checked += count as u64 * 4;
        done += count;
        if !control.poll() {
            return false;
        }
    }
    control.after_write(region);
    true
}

/// A seeded pseudo-random fill and check, then the complement of the same
/// stream. Different seeds give different data, so each pass tests new values.
pub fn random_test(region: &Region, errors: &mut Errors, control: &mut dyn Control, seed: u32) -> bool {
    for invert in [0, u32::MAX] {
        let mut state = usable_seed(seed);
        if !fill(region, control, |_| next_random(&mut state) ^ invert) {
            return false;
        }
        let mut state = usable_seed(seed);
        if !verify(region, errors, control, Test::Random, |_| next_random(&mut state) ^ invert) {
            return false;
        }
    }
    true
}


/// Fills the region with a seeded pseudo-random stream (the soak mode's
/// allocator-churn test: every block gets its own seed, so a block that
/// overlaps another one's memory is overwritten with the wrong data).
pub fn stream_fill(region: &Region, control: &mut dyn Control, seed: u32) -> bool {
    let mut state = usable_seed(seed);
    fill(region, control, |_| next_random(&mut state))
}

/// Checks the region still holds the stream `stream_fill` wrote for `seed`.
pub fn stream_verify(
    region: &Region,
    errors: &mut Errors,
    control: &mut dyn Control,
    test: Test,
    seed: u32,
) -> bool {
    let mut state = usable_seed(seed);
    verify(region, errors, control, test, |_| next_random(&mut state))
}

/// A small, repeatable pseudo-random generator for the tape's own choices
/// (block sizes, shuffles, frame fill); the same xorshift32 the tests use.
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Rng(usable_seed(seed))
    }

    pub fn next(&mut self) -> u32 {
        next_random(&mut self.0)
    }

    /// A value in `0..bound` (`bound` must be non-zero).
    pub fn below(&mut self, bound: u32) -> u32 {
        self.next() % bound
    }
}

/// Fills `buffer` with the byte stream for `seed` (the disk check's file body).
pub fn fill_bytes(buffer: &mut [u8], seed: u32) {
    let mut state = usable_seed(seed);
    for chunk in buffer.chunks_mut(4) {
        let word = next_random(&mut state).to_le_bytes();
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
}

/// Index of the first byte of `buffer` that differs from the stream for
/// `seed`, or `None` if it matches.
pub fn first_mismatch(buffer: &[u8], seed: u32) -> Option<usize> {
    let mut state = usable_seed(seed);
    for (chunk_index, chunk) in buffer.chunks(4).enumerate() {
        let word = next_random(&mut state).to_le_bytes();
        if let Some(offset) = chunk.iter().zip(word).position(|(have, want)| *have != want) {
            return Some(chunk_index * 4 + offset);
        }
    }
    None
}

/// Watches the two clocks the kernel offers a tape: `micros()` (the TSC when
/// the CPU has an invariant one, otherwise the PIT) and `ticks()` (the 100 Hz
/// timer interrupt count). Their difference should stay put. A drifting
/// difference says the calibration was off; a tick count that stops while
/// microseconds pass says timer interrupts are being lost -- the sort of silent
/// failure a long, busy run can provoke.
pub struct ClockMonitor {
    first_micros: u64,
    first_ticks: u64,
    last_micros: u64,
    last_ticks: u64,
    pub samples: u32,
    /// `micros()` went backwards between two samples.
    pub backwards: u32,
    /// A sample at least a second after the previous one saw no new ticks.
    pub stalls: u32,
    /// Largest `|micros elapsed - ticks elapsed * 10000|` seen, microseconds.
    pub max_skew_us: u64,
    /// The same difference at the latest sample, signed.
    pub skew_us: i64,
}

impl ClockMonitor {
    pub const fn new() -> Self {
        ClockMonitor {
            first_micros: 0,
            first_ticks: 0,
            last_micros: 0,
            last_ticks: 0,
            samples: 0,
            backwards: 0,
            stalls: 0,
            max_skew_us: 0,
            skew_us: 0,
        }
    }

    pub fn sample(&mut self, micros: u64, ticks: u64) {
        if self.samples == 0 {
            self.first_micros = micros;
            self.first_ticks = ticks;
        } else {
            if micros < self.last_micros {
                self.backwards += 1;
            }
            if micros.saturating_sub(self.last_micros) >= 1_000_000 && ticks == self.last_ticks {
                self.stalls += 1;
            }
        }
        self.samples += 1;
        self.last_micros = micros;
        self.last_ticks = ticks;
        let by_micros = micros as i64 - self.first_micros as i64;
        let by_ticks = (ticks as i64 - self.first_ticks as i64) * 10_000;
        self.skew_us = by_micros - by_ticks;
        self.max_skew_us = self.max_skew_us.max(self.skew_us.unsigned_abs());
    }

    /// The latest skew as parts per million of the elapsed time (0 at the start).
    pub fn drift_ppm(&self) -> i64 {
        let elapsed = self.last_micros.saturating_sub(self.first_micros) as i64;
        if elapsed == 0 { 0 } else { self.skew_us * 1_000_000 / elapsed }
    }
}
