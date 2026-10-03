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
}

impl Test {
    pub fn name(self) -> &'static [u8] {
        match self {
            Test::Address => b"addr",
            Test::Walking => b"walk",
            Test::Moving => b"moving",
            Test::Random => b"random",
            Test::Retention => b"hold",
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
}
