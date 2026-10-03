//! The test engine behind the `memtest` tape: pure functions over a region of
//! RAM, with no kernel callbacks, so the logic can be read (and unit tested on
//! the host) on its own.
//!
//! RAM faults come in a few flavours, and each test here is aimed at one:
//!
//! * **Address test** -- every word holds its own physical address, so a stuck
//!   or shorted address line (two addresses landing on one cell) shows up as a
//!   word holding someone else's address. Run once plain and once inverted.
//! * **Walking ones / zeros** -- one bit set (or clear) in an otherwise empty
//!   word, for each of the 32 bit positions: finds stuck data lines and cells
//!   that cannot hold a 0 or a 1 at that position.
//! * **Moving inversions** -- fill with a pattern, then sweep up verifying and
//!   writing the complement, then sweep down doing the reverse. Neighbouring
//!   cells are disturbed between a write and its read-back, which catches
//!   coupling faults a plain fill-and-check never would.
//! * **Random** -- a seeded pseudo-random fill and its complement; the same
//!   seed regenerates the expected values for the check. Catches pattern
//!   sensitivity the fixed patterns miss.
//!
//! Every access is volatile: the compiler must not be allowed to notice that
//! it just wrote a value and "know" what a read will return.
//!
//! Errors are counted, never fatal: [`Errors`] keeps the first few mismatches,
//! a per-bit flip histogram and a table of which 1 MiB blocks went bad, so a
//! failing module shows up as a pattern (one stuck bit? one block?) rather
//! than a wall of lines.
#![cfg_attr(not(test), no_std)]

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
    pub address: u32,
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
    pub bad_blocks: [(u32, u32); MAX_BAD_BLOCKS],
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

    fn record(&mut self, address: u32, expected: u32, actual: u32, test: Test) {
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
        // One line per bad address: a stuck cell fails every test, and the
        // repeats would push the other bad addresses off the list.
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
    base: u32,
}

impl Region {
    /// # Safety
    /// `ptr` must be valid for reads and writes of `words` 32-bit words and
    /// not used by anything else while the region is tested. `base` is the
    /// physical address the first word has (the address test stores it).
    pub unsafe fn new(ptr: *mut u32, words: usize, base: u32) -> Self {
        Region { ptr, words, base }
    }

    pub fn words(&self) -> usize {
        self.words
    }

    pub fn base(&self) -> u32 {
        self.base
    }

    fn address(&self, index: usize) -> u32 {
        self.base.wrapping_add((index as u32).wrapping_mul(4))
    }

    fn read(&self, index: usize) -> u32 {
        debug_assert!(index < self.words);
        unsafe { read_volatile(self.ptr.add(index)) }
    }

    fn write(&self, index: usize, value: u32) {
        debug_assert!(index < self.words);
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
                errors.record(region.address(i), expected, actual, test);
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
                errors.record(region.address(i), expected, actual, Test::Moving);
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

/// Ethertype of the soak traffic (an experimental value, like the other test
/// tapes' own).
pub const SOAK_ETHERTYPE: u16 = 0x88bb;
/// Smallest frame the generator builds (Ethernet's minimum without the FCS).
pub const MIN_FRAME: usize = 60;
const FRAME_MAGIC: [u8; 4] = *b"FSOK";
const FRAME_FILLER_AT: usize = 28;

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in bytes {
        hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
    }
    hash
}

/// Builds one soak frame of `total_len` bytes (clamped to `MIN_FRAME..=out.len()`)
/// into `out` and returns that length. Layout: Ethernet header, `FSOK`, the
/// sequence number (u32), the real frame length (u16, because short frames are
/// padded on the wire), an FNV-1a checksum (u32) of the filler, then pseudo-random
/// filler seeded by the sequence number. The length is what makes a truncated
/// frame detectable; the checksum catches flipped bits anywhere in the filler.
pub fn build_frame(out: &mut [u8], destination: [u8; 6], source: [u8; 6], sequence: u32, total_len: usize) -> usize {
    let total = total_len.clamp(MIN_FRAME, out.len());
    out[0..6].copy_from_slice(&destination);
    out[6..12].copy_from_slice(&source);
    out[12..14].copy_from_slice(&SOAK_ETHERTYPE.to_be_bytes());
    out[14..18].copy_from_slice(&FRAME_MAGIC);
    out[18..22].copy_from_slice(&sequence.to_le_bytes());
    out[22..24].copy_from_slice(&(total as u16).to_le_bytes());
    fill_bytes(&mut out[FRAME_FILLER_AT..total], sequence ^ 0x5a5a_a5a5);
    let checksum = fnv1a(&out[FRAME_FILLER_AT..total]);
    out[24..28].copy_from_slice(&checksum.to_le_bytes());
    total
}

/// What `check_frame` made of a received frame.
#[derive(Debug, PartialEq, Eq)]
pub enum FrameCheck {
    /// Not a soak frame (ARP, discovery, a `term net` session...): ignore it.
    NotOurs,
    /// A soak frame whose length or checksum is wrong.
    Corrupt,
    Good(u32),
}

pub fn check_frame(frame: &[u8]) -> FrameCheck {
    if frame.len() < FRAME_FILLER_AT
        || frame[12..14] != SOAK_ETHERTYPE.to_be_bytes()
        || frame[14..18] != FRAME_MAGIC
    {
        return FrameCheck::NotOurs;
    }
    let sequence = u32::from_le_bytes([frame[18], frame[19], frame[20], frame[21]]);
    let declared = u16::from_le_bytes([frame[22], frame[23]]) as usize;
    let checksum = u32::from_le_bytes([frame[24], frame[25], frame[26], frame[27]]);
    if declared != frame.len().max(MIN_FRAME).min(declared) || declared > frame.len() || declared < MIN_FRAME {
        return FrameCheck::Corrupt;
    }
    if fnv1a(&frame[FRAME_FILLER_AT..declared]) != checksum
        || first_mismatch(&frame[FRAME_FILLER_AT..declared], sequence ^ 0x5a5a_a5a5).is_some()
    {
        return FrameCheck::Corrupt;
    }
    FrameCheck::Good(sequence)
}

/// Counters for one direction of the soak traffic.
#[derive(Clone, Copy, Default)]
pub struct LinkStats {
    pub sent: u32,
    pub send_failures: u32,
    pub received: u32,
    pub corrupt: u32,
    /// Sequence numbers skipped (frames that never arrived, so far).
    pub lost: u32,
    /// Frames that arrived twice or after a later one.
    pub out_of_order: u32,
    expected: u32,
}

impl LinkStats {
    /// Records a good frame with this sequence number.
    pub fn on_good(&mut self, sequence: u32) {
        self.received += 1;
        if sequence == self.expected {
            self.expected += 1;
        } else if sequence > self.expected {
            self.lost += sequence - self.expected;
            self.expected = sequence + 1;
        } else {
            self.out_of_order += 1;
        }
    }

    /// Frames still unaccounted for: gaps minus frames that turned up late.
    /// (A duplicate also counts as "late", so a duplicated frame can hide one
    /// missing frame; both show in `out_of_order` anyway.)
    pub fn missing(&self) -> u32 {
        self.lost.saturating_sub(self.out_of_order)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts polls, optionally flips one bit after each write sweep, and can
    /// abort after a number of polls.
    struct Probe {
        polls: u32,
        abort_after: u32,
        inject: Option<(usize, u32)>,
        injected: u32,
    }

    impl Probe {
        fn new() -> Self {
            Probe { polls: 0, abort_after: u32::MAX, inject: None, injected: 0 }
        }
    }

    impl Control for Probe {
        fn poll(&mut self) -> bool {
            self.polls += 1;
            self.polls <= self.abort_after
        }
        fn after_write(&mut self, region: &Region) {
            if let Some((index, mask)) = self.inject {
                region.corrupt(index, mask);
                self.injected += 1;
            }
        }
    }

    fn region(buffer: &mut Vec<u32>, base: u32) -> Region {
        unsafe { Region::new(buffer.as_mut_ptr(), buffer.len(), base) }
    }

    type TestFn = fn(&Region, &mut Errors, &mut dyn Control) -> bool;

    fn all_tests() -> [(&'static str, TestFn); 4] {
        [
            ("address", address_test),
            ("walking", walking_test),
            ("moving", moving_test),
            ("random", |r, e, c| random_test(r, e, c, 7)),
        ]
    }

    #[test]
    fn clean_memory_reports_nothing() {
        for (name, test) in all_tests() {
            let mut buffer = vec![0u32; 3 * BLOCK_WORDS / 2 + 5]; // not a whole number of blocks
            let region = region(&mut buffer, 0x0100_0000);
            let mut errors = Errors::new();
            assert!(test(&region, &mut errors, &mut Probe::new()), "{name}");
            assert_eq!(errors.total, 0, "{name}");
            assert!(errors.bytes_checked >= buffer.len() as u64 * 4, "{name} checked too little");
        }
    }

    #[test]
    fn an_injected_bit_flip_is_found_by_every_test() {
        for (name, test) in all_tests() {
            let mut buffer = vec![0u32; 1000];
            let region = region(&mut buffer, 0x0200_0000);
            let mut errors = Errors::new();
            let mut probe = Probe::new();
            probe.inject = Some((123, 0x0000_0008));
            assert!(test(&region, &mut errors, &mut probe), "{name}");
            assert!(probe.injected > 0 && errors.total >= probe.injected, "{name}: {} of {}", errors.total, probe.injected);
            // Every recorded mismatch is the injected word and bit.
            let first = errors.recorded[0];
            assert_eq!(first.address, 0x0200_0000 + 123 * 4, "{name}");
            assert_eq!(first.expected ^ first.actual, 8, "{name}");
            assert_eq!(errors.bit_flips[3], errors.total, "{name}");
            assert_eq!(errors.bad_block_count, 1, "{name}");
            assert_eq!(errors.bad_blocks[0].0, 0x0200_0000 >> 20, "{name}");
        }
    }

    #[test]
    fn a_stuck_bit_is_seen_by_the_walking_test_only_where_it_matters() {
        // A cell stuck at 1 in bit 5 cannot be "tested" by writing zero to it:
        // simulate by corrupting after each write sweep, only when bit 5 is 0.
        struct Stuck;
        impl Control for Stuck {
            fn poll(&mut self) -> bool {
                true
            }
            fn after_write(&mut self, region: &Region) {
                let value = region.read(10);
                region.write(10, value | 1 << 5);
            }
        }
        let mut buffer = vec![0u32; 64];
        let region = region(&mut buffer, 0);
        let mut errors = Errors::new();
        assert!(walking_test(&region, &mut errors, &mut Stuck));
        // Every pattern with bit 5 clear fails: walking ones at 31 other bits
        // and walking zeros only at bit 5 itself.
        assert_eq!(errors.total, 31 + 1);
        assert_eq!(errors.bit_flips[5], 32);
        assert!(errors.recorded.iter().take(errors.recorded_count).all(|m| m.address == 40));
    }

    #[test]
    fn address_test_notices_two_addresses_sharing_a_cell() {
        // Words 3 and 7 alias: writing one is visible in the other.
        struct Alias;
        impl Control for Alias {
            fn poll(&mut self) -> bool {
                true
            }
            fn after_write(&mut self, region: &Region) {
                let value = region.read(3);
                region.write(7, value);
            }
        }
        let mut buffer = vec![0u32; 16];
        let region = region(&mut buffer, 0x1000);
        let mut errors = Errors::new();
        assert!(address_test(&region, &mut errors, &mut Alias));
        assert_eq!(errors.total, 2);
        assert_eq!(errors.recorded[0].address, 0x1000 + 7 * 4);
        assert_eq!(errors.recorded[0].actual, 0x1000 + 3 * 4);
    }

    #[test]
    fn an_abort_stops_the_test_early() {
        let mut buffer = vec![0u32; 4 * BLOCK_WORDS];
        let region = region(&mut buffer, 0);
        let mut errors = Errors::new();
        let mut probe = Probe::new();
        probe.abort_after = 2;
        assert!(!moving_test(&region, &mut errors, &mut probe));
        assert_eq!(probe.polls, 3);
    }

    #[test]
    fn the_same_bad_address_is_listed_once() {
        let mut errors = Errors::new();
        for test in [Test::Address, Test::Moving, Test::Random] {
            errors.record(0x1000, 0, 8, test);
        }
        errors.record(0x2000, 0, 8, Test::Random);
        assert_eq!(errors.total, 4);
        assert_eq!(errors.recorded_count, 2);
    }

    #[test]
    fn error_bookkeeping_keeps_counting_past_its_tables() {
        let mut errors = Errors::new();
        for index in 0..(MAX_BAD_BLOCKS as u32 + 5) {
            errors.record(index << 20, 0, 1, Test::Random); // distinct addresses
        }
        assert_eq!(errors.total, MAX_BAD_BLOCKS as u32 + 5);
        assert_eq!(errors.recorded_count, MAX_RECORDED);
        assert_eq!(errors.bad_block_count, MAX_BAD_BLOCKS);
        assert_eq!(errors.bit_flips[0], errors.total);
    }

    #[test]
    fn random_seeds_give_different_data() {
        let mut state_a = usable_seed(1);
        let mut state_b = usable_seed(2);
        assert_ne!(next_random(&mut state_a), next_random(&mut state_b));
        assert_ne!(usable_seed(0), 0);
    }

    #[test]
    fn soak_frames_round_trip_at_every_length() {
        let mut buffer = [0u8; 1514];
        for length in [60, 61, 99, 512, 1513, 1514] {
            let built = build_frame(&mut buffer, [1; 6], [2; 6], 77 + length as u32, length);
            assert_eq!(built, length);
            assert_eq!(check_frame(&buffer[..built]), FrameCheck::Good(77 + length as u32));
        }
        // Ethernet pads a short frame on the wire: trailing zeros must not matter.
        let built = build_frame(&mut buffer, [1; 6], [2; 6], 5, 60);
        assert_eq!(check_frame(&buffer[..built + 4]), FrameCheck::Good(5));
    }

    #[test]
    fn soak_frames_catch_flipped_bits_truncation_and_strangers() {
        let mut buffer = [0u8; 1514];
        let built = build_frame(&mut buffer, [1; 6], [2; 6], 9, 800);
        for position in [28, 29, 400, built - 1] {
            let mut damaged = buffer;
            damaged[position] ^= 0x10;
            assert_eq!(check_frame(&damaged[..built]), FrameCheck::Corrupt, "bit flip at {position}");
        }
        assert_eq!(check_frame(&buffer[..built - 1]), FrameCheck::Corrupt, "truncated");
        let mut stranger = buffer;
        stranger[12] = 0x08; // IPv4's ethertype
        stranger[13] = 0x00;
        assert_eq!(check_frame(&stranger[..built]), FrameCheck::NotOurs);
        assert_eq!(check_frame(&[0u8; 20]), FrameCheck::NotOurs);
    }

    #[test]
    fn link_stats_count_gaps_and_late_frames() {
        let mut link = LinkStats::default();
        for sequence in [0, 1, 2, 5, 3, 6] {
            link.on_good(sequence);
        }
        // 3 and 4 were skipped when 5 arrived; 3 then showed up late; 4 never did.
        assert_eq!((link.received, link.lost, link.out_of_order, link.missing()), (6, 2, 1, 1));
    }

    #[test]
    fn the_clock_monitor_sees_drift_stalls_and_reversals() {
        let mut clock = ClockMonitor::new();
        clock.sample(1_000_000, 100);
        clock.sample(2_000_000, 200);
        assert_eq!((clock.skew_us, clock.backwards, clock.stalls), (0, 0, 0));
        clock.sample(3_001_000, 300); // micros ran 1 ms fast
        assert_eq!(clock.skew_us, 1000);
        assert_eq!(clock.drift_ppm(), 499); // 1 ms over 2.001 s
        clock.sample(4_001_000, 300); // a second passed, no tick arrived
        assert_eq!(clock.stalls, 1);
        clock.sample(4_000_000, 301);
        assert_eq!(clock.backwards, 1);
        assert_eq!(clock.max_skew_us, 1_000_000_u64.max(clock.max_skew_us));
    }

    #[test]
    fn byte_streams_match_their_seed_only() {
        let mut data = [0u8; 1001]; // not a multiple of four
        fill_bytes(&mut data, 42);
        assert_eq!(first_mismatch(&data, 42), None);
        assert!(first_mismatch(&data, 43).is_some());
        data[1000] ^= 1;
        assert_eq!(first_mismatch(&data, 42), Some(1000));
    }

    #[test]
    fn churn_blocks_with_different_seeds_detect_overlap() {
        let mut buffer = vec![0u32; 5000];
        let whole = region(&mut buffer, 0);
        let first = unsafe { Region::new(buffer.as_mut_ptr(), 3000, 0) };
        let second = unsafe { Region::new(buffer.as_mut_ptr().add(2000), 3000, 8000) }; // overlaps by 1000 words
        let _ = whole;
        let mut errors = Errors::new();
        let mut control = Probe::new();
        assert!(stream_fill(&first, &mut control, 1));
        assert!(stream_fill(&second, &mut control, 2));
        assert!(stream_verify(&second, &mut errors, &mut control, Test::Churn, 2));
        assert_eq!(errors.total, 0);
        assert!(stream_verify(&first, &mut errors, &mut control, Test::Churn, 1));
        assert_eq!(errors.total, 1000);
    }
}
