//! `memtest`: a RAM tester and long-run soak test that runs as a tape.
//!
//! ```text
//! tape run memtest [size=MB] [passes=N] [tests=addr,walk,moving,random]
//!                  [hold=SECONDS] [frames=on|off] [seed=N] [inject=WORD]
//! tape run memtest soak [for=90|5m|2h] [status=S] [net=A,B] [net.fps=N]
//!                  [churn=on|off] [disk=soakNAME [disk.kb=N] [disk.every=S]]
//!                  ... plus any memory option above
//! ```
//!
//! **Memory mode.** It claims every free block of RAM it can get from the
//! kernel -- the 4 KiB frame pool through `alloc_frames` (ABI slot 39) and the
//! 1 MiB big pool through `alloc` -- up to `size=` MiB in total (default:
//! everything), runs the selected tests over each block and reports mismatches
//! with their physical addresses. `passes=0` repeats until Escape. The kernel
//! keeps a 4 MiB reserve of frames for itself, and the tape's own 4 MiB, the
//! kernel's low-memory region and anything above 2 GiB (unmapped) are out of
//! reach, so "no errors" covers what was claimed, which the tape prints up front.
//!
//! **Soak mode** is for "how does the whole OS hold up under long, heavy use".
//! The memory tests run on repeat (until `for=` elapses or Escape), and the
//! engine's per-256 KiB `poll` hook doubles as the scheduler for the rest, since
//! a tape is single-threaded:
//!
//! * `churn` (default on): between passes every claimed block is freed and a
//!   batch of random-sized blocks is allocated, filled with per-block random
//!   data, checked and freed in a different order, then everything is claimed
//!   again. Overlapping allocations show up as corrupt data; memory that does
//!   not come back shows up as a smaller claim or a changed free-frame count.
//! * `net=A,B`: two NICs cabled to each other (one-based, as `nic list`), each
//!   sending checksummed frames of varying size to the other at `net.fps`
//!   frames/s, with sequence numbers to count loss. Keep `term net` on another NIC.
//! * clock: `micros()` against the 100 Hz tick, catching drift, lost timer
//!   interrupts and a clock that runs backwards.
//! * kernel health: free frames and big-pool chunks before and after, and the
//!   NIC watchdog recovery/overrun counters (ABI slot 41).
//! * `disk=NAME` is **never on by itself**: the machine may boot from a USB
//!   flash stick, and a disk test writes to it. With it, every `disk.every`
//!   seconds (default 120) a `disk.kb` KiB (default 32) pseudo-random file is
//!   written through the filesystem, read back and compared. The name must start
//!   with `soak` (so a real file can never be overwritten) and may name another
//!   disk, e.g. `disk2/soak.tmp`; the file is removed at the end.
//!
//! A status line prints every `status=` seconds (default 10) and the run ends
//! with a PASS/FAIL verdict listing every reason. The engine is in `lib.rs`.
#![no_std]
#![no_main]

extern crate memtest_tape;

use core::ptr::addr_of_mut;
use memtest_tape::{
    ClockMonitor, Control, Errors, FrameCheck, LinkStats, MIN_FRAME, Region, Rng, Test, address_test, build_frame,
    check_frame, fill_bytes, fill_pattern, first_mismatch, moving_test, random_test, stream_fill, stream_verify,
    verify_pattern, walking_test,
};
use tape_rt::{print_bytes, print_u32};

type Putc = extern "C" fn(u32);

const MAX_REGIONS: usize = 32;
const MIB: u32 = 1024 * 1024;
/// The frame pool is tracked up to 128 MiB, the big pool up to 2 GiB.
const FRAME_POOL_MAX: u32 = 128 * MIB;
const BIG_POOL_MAX: u32 = 2048 * MIB;
/// Smallest block worth claiming (a request that fails below this ends the search).
const MIN_FRAME_BLOCK: u32 = 64 * 1024;
/// Blocks allocated at once in one churn round.
const CHURN_SLOTS: usize = 8;
/// Largest disk-check file; the two buffers below are this big.
const DISK_MAX_KIB: u32 = 128;
const FRAME_BUFFER: usize = 1536;
const FS_OK: u32 = 0;
/// `kernel_stat` ids (ABI slot 41).
const STAT_FREE_FRAMES: u32 = 0;
const STAT_FREE_BIG: u32 = 1;
const STAT_NIC_RECOVERIES: u32 = 2;
const STAT_NIC_OVERRUNS: u32 = 3;

// A tape runs on the 64 KiB kernel stack, so every sizeable buffer is static.
static mut TX_FRAME: [u8; FRAME_BUFFER] = [0; FRAME_BUFFER];
static mut RX_FRAME: [u8; FRAME_BUFFER] = [0; FRAME_BUFFER];
static mut DISK_WRITE: [u8; (DISK_MAX_KIB * 1024) as usize] = [0; (DISK_MAX_KIB * 1024) as usize];
static mut DISK_READ: [u8; (DISK_MAX_KIB * 1024) as usize] = [0; (DISK_MAX_KIB * 1024) as usize];

#[derive(Clone, Copy)]
struct Block {
    address: u32,
    bytes: u32,
    from_frames: bool,
}

const NO_BLOCK: Block = Block { address: 0, bytes: 0, from_frames: false };

struct Options<'a> {
    size_mib: Option<u32>,
    passes: u32,
    passes_given: bool,
    walk: bool,
    addr: bool,
    moving: bool,
    random: bool,
    hold_s: u32,
    frames: bool,
    seed: u32,
    inject: Option<usize>,
    soak: bool,
    for_s: u32,
    status_s: u32,
    net: Option<(u32, u32)>,
    net_fps: u32,
    churn: bool,
    disk: Option<&'a [u8]>,
    disk_kb: u32,
    disk_every_s: u32,
}

/// The kernel callbacks the soak services use.
#[derive(Clone, Copy)]
struct Env {
    putc: Putc,
    micros: extern "C" fn() -> u64,
    ticks: extern "C" fn() -> u64,
    kernel_stat: extern "C" fn(u32, u32) -> u32,
    nic_count: extern "C" fn() -> u32,
    nic_mac_at: extern "C" fn(u32, u32) -> u32,
    nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_max_frame_len: extern "C" fn() -> u32,
    fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    fs_remove: extern "C" fn(u32, u32) -> u32,
}

/// Two cabled NICs exchanging checksummed frames, one stream per direction.
struct NetPair {
    nic: [u32; 2],
    mac: [[u8; 6]; 2],
    fps: u32,
    max_frame: usize,
    started_us: u64,
    /// `stats[d]`: the stream sent by `nic[d]` and received by `nic[1 - d]`.
    stats: [LinkStats; 2],
    sequence: [u32; 2],
    recoveries_at_start: [u32; 2],
    overruns_at_start: [u32; 2],
}

struct DiskCheck<'a> {
    name: &'a [u8],
    bytes: u32,
    every_us: u64,
    next_us: u64,
    rounds: u32,
    ok: u32,
    mismatches: u32,
    errors: u32,
    last_error: u32,
    written: u64,
}

struct Soak<'a> {
    env: Env,
    started_us: u64,
    end_us: u64,
    status_every_us: u64,
    next_status_us: u64,
    next_clock_us: u64,
    clock: ClockMonitor,
    net: Option<NetPair>,
    disk: Option<DiskCheck<'a>>,
    /// Updated by `main` after each test, for the status line.
    pass: u32,
    mem_errors: u32,
}

/// The engine's view of the kernel: abort checks (which also keep the
/// watchdog fed), the optional fault injection and, in soak mode, the
/// background services.
struct Kernel<'a> {
    should_abort: extern "C" fn() -> u32,
    inject: Option<usize>,
    aborted: bool,
    timed_out: bool,
    soak: Option<Soak<'a>>,
}

impl Control for Kernel<'_> {
    fn poll(&mut self) -> bool {
        if (self.should_abort)() != 0 {
            self.aborted = true;
        }
        if let Some(soak) = self.soak.as_mut() {
            if soak.service() {
                self.timed_out = true;
            }
        }
        !(self.aborted || self.timed_out)
    }

    fn after_write(&mut self, region: &Region) {
        if let Some(index) = self.inject {
            region.corrupt(index, 0x0000_0008);
        }
    }
}

impl Soak<'_> {
    /// Runs whatever is due; returns `true` once the time limit has passed.
    /// Called every 256 KiB of memory work, so it must stay cheap when idle.
    fn service(&mut self) -> bool {
        let env = self.env;
        let now = (env.micros)();
        if self.net.is_some() {
            self.service_net(now);
        }
        if now >= self.next_clock_us {
            self.next_clock_us = now + 1_000_000;
            self.clock.sample(now, (env.ticks)());
        }
        if let Some(disk) = self.disk.as_mut() {
            if now >= disk.next_us {
                disk.next_us = now + disk.every_us;
                disk.check(&env);
            }
        }
        if now >= self.next_status_us {
            self.next_status_us = now + self.status_every_us;
            self.print_status(now);
        }
        self.end_us != 0 && now >= self.end_us
    }

    fn service_net(&mut self, now: u64) {
        let env = self.env;
        let Some(net) = self.net.as_mut() else { return };
        let tx = addr_of_mut!(TX_FRAME) as u32;
        let rx = addr_of_mut!(RX_FRAME) as u32;
        // Send what is due, at most 16 frames per direction per call.
        let due = (now - net.started_us) * u64::from(net.fps) / 1_000_000;
        for direction in 0..2 {
            let mut burst = 0;
            while u64::from(net.sequence[direction]) < due && burst < 16 {
                let sequence = net.sequence[direction];
                // A pseudo-random length from 60 bytes up to the NIC's limit.
                let span = (net.max_frame - MIN_FRAME + 1) as u32;
                let length = MIN_FRAME + (sequence.wrapping_mul(2_654_435_761) >> 8) as usize % span as usize;
                let frame = unsafe { core::slice::from_raw_parts_mut(tx as *mut u8, FRAME_BUFFER) };
                let built = build_frame(frame, net.mac[1 - direction], net.mac[direction], sequence, length);
                if (env.nic_send_at)(net.nic[direction], tx, built as u32) == 0 {
                    net.stats[direction].send_failures += 1;
                    break; // try again later, with the same sequence number
                }
                net.stats[direction].sent += 1;
                net.sequence[direction] += 1;
                burst += 1;
            }
        }
        // Drain both receive rings.
        for receiver in 0..2 {
            for _ in 0..16 {
                let length = (env.nic_receive_at)(net.nic[receiver], rx, net.max_frame as u32) as usize;
                if length < 14 {
                    break;
                }
                let frame = unsafe { core::slice::from_raw_parts(rx as *const u8, length.min(FRAME_BUFFER)) };
                let sender = 1 - receiver;
                if frame[6..12] != net.mac[sender] {
                    continue; // somebody else's traffic
                }
                match check_frame(frame) {
                    FrameCheck::Good(sequence) => net.stats[sender].on_good(sequence),
                    FrameCheck::Corrupt => net.stats[sender].corrupt += 1,
                    FrameCheck::NotOurs => {}
                }
            }
        }
    }

    fn print_status(&self, now: u64) {
        let putc = self.env.putc;
        print_clock(putc, (now - self.started_us) / 1_000_000);
        print_bytes(putc, b" pass ");
        print_u32(putc, self.pass);
        print_bytes(putc, b" errors ");
        print_u32(putc, self.mem_errors);
        print_bytes(putc, b" | clock skew ");
        print_signed(putc, self.clock.skew_us);
        print_bytes(putc, b" us | free frames ");
        print_u32(putc, (self.env.kernel_stat)(STAT_FREE_FRAMES, 0));
        print_bytes(putc, b"\n");
        if let Some(net) = &self.net {
            print_bytes(putc, b"         net tx ");
            print_u32(putc, net.stats[0].sent + net.stats[1].sent);
            print_bytes(putc, b" rx ");
            print_u32(putc, net.stats[0].received + net.stats[1].received);
            print_bytes(putc, b" missing ");
            print_u32(putc, net.stats[0].missing() + net.stats[1].missing());
            print_bytes(putc, b" bad ");
            print_u32(putc, net.stats[0].corrupt + net.stats[1].corrupt);
            print_bytes(putc, b"\n");
        }
        if let Some(disk) = &self.disk {
            print_bytes(putc, b"         disk ok ");
            print_u32(putc, disk.ok);
            print_bytes(putc, b" bad ");
            print_u32(putc, disk.mismatches);
            print_bytes(putc, b" errors ");
            print_u32(putc, disk.errors);
            print_bytes(putc, b"\n");
        }
    }
}

impl DiskCheck<'_> {
    /// One round: write a pseudo-random file, read it back, compare. Only ever
    /// reached when the operator asked for `disk=`.
    fn check(&mut self, env: &Env) {
        self.rounds += 1;
        let length = self.bytes as usize;
        let seed = self.rounds.wrapping_mul(2_654_435_761) | 1;
        let write = unsafe { core::slice::from_raw_parts_mut(addr_of_mut!(DISK_WRITE) as *mut u8, length) };
        let read = unsafe { core::slice::from_raw_parts_mut(addr_of_mut!(DISK_READ) as *mut u8, length) };
        fill_bytes(write, seed);
        let name = self.name.as_ptr() as u32;
        let name_length = self.name.len() as u32;
        let status = (env.fs_write)(name, name_length, write.as_ptr() as u32, self.bytes);
        if status != FS_OK {
            self.errors += 1;
            self.last_error = status;
            return;
        }
        self.written += u64::from(self.bytes);
        let mut got = 0u32;
        let status = (env.fs_read)(name, name_length, read.as_ptr() as u32, self.bytes, &mut got as *mut u32 as u32);
        if status != FS_OK || got != self.bytes {
            self.errors += 1;
            self.last_error = status;
        } else if first_mismatch(read, seed).is_some() {
            self.mismatches += 1;
        } else {
            self.ok += 1;
        }
        read.fill(0); // so a later failed read cannot "pass" on stale data
    }
}

fn parse_u32(text: &[u8]) -> Option<u32> {
    if text.is_empty() {
        return None;
    }
    let mut value = 0u32;
    for &byte in text {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

/// `90` (seconds), `5m` or `2h`.
fn parse_duration(text: &[u8]) -> Option<u32> {
    match text.split_last()? {
        (b'm', digits) => parse_u32(digits)?.checked_mul(60),
        (b'h', digits) => parse_u32(digits)?.checked_mul(3600),
        (b's', digits) => parse_u32(digits),
        _ => parse_u32(text),
    }
}

/// `soak.tmp`, `soak-1`, `disk2/soak.tmp`: a file name starting with `soak`
/// (optionally behind a `diskN/` selector), in FOSFS's lowercase alphabet.
fn valid_soak_file(name: &[u8]) -> bool {
    let base = match name.iter().position(|&byte| byte == b'/') {
        Some(slash) => {
            let selector = &name[..slash];
            if !(selector.len() > 4 && selector.starts_with(b"disk") && selector[4..].iter().all(u8::is_ascii_digit)) {
                return false;
            }
            &name[slash + 1..]
        }
        None => name,
    };
    name.len() <= 32
        && base.len() <= 16
        && base.starts_with(b"soak")
        && base.iter().all(|&byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(&byte))
}

fn parse_options(arguments: &[u8]) -> Option<Options<'_>> {
    let mut options = Options {
        size_mib: None,
        passes: 1,
        passes_given: false,
        walk: true,
        addr: true,
        moving: true,
        random: true,
        hold_s: 0,
        frames: true,
        seed: 1,
        inject: None,
        soak: false,
        for_s: 0,
        status_s: 10,
        net: None,
        net_fps: 500,
        churn: true,
        disk: None,
        disk_kb: 32,
        disk_every_s: 120,
    };
    // The kernel passes the text after `tape run`, which starts with the
    // tape's own name.
    let words = arguments.split(|&byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
    for word in words.filter(|word| !word.is_empty() && *word != b"memtest") {
        if word == b"soak" {
            options.soak = true;
            continue;
        }
        let equals = word.iter().position(|&byte| byte == b'=')?;
        let (key, value) = (&word[..equals], &word[equals + 1..]);
        match key {
            b"size" => options.size_mib = Some(parse_u32(value).filter(|&mib| mib >= 1)?),
            b"passes" => {
                options.passes = parse_u32(value)?;
                options.passes_given = true;
            }
            b"hold" => options.hold_s = parse_u32(value)?,
            b"seed" => options.seed = parse_u32(value)?,
            b"inject" => options.inject = Some(parse_u32(value)? as usize),
            b"for" => options.for_s = parse_duration(value)?,
            b"status" => options.status_s = parse_duration(value).filter(|&seconds| seconds >= 1)?,
            b"net" => {
                let comma = value.iter().position(|&byte| byte == b',')?;
                let first = parse_u32(&value[..comma]).filter(|&nic| nic >= 1)?;
                let second = parse_u32(&value[comma + 1..]).filter(|&nic| nic >= 1)?;
                options.net = Some((first - 1, second - 1));
            }
            b"net.fps" => options.net_fps = parse_u32(value).filter(|&fps| (1..=20_000).contains(&fps))?,
            b"churn" => options.churn = parse_switch(value)?,
            b"frames" => options.frames = parse_switch(value)?,
            b"disk" => options.disk = Some(value).filter(|name| valid_soak_file(name)),
            b"disk.kb" => options.disk_kb = parse_u32(value).filter(|&kib| (1..=DISK_MAX_KIB).contains(&kib))?,
            b"disk.every" => options.disk_every_s = parse_duration(value).filter(|&seconds| seconds >= 5)?,
            b"tests" => {
                options.addr = false;
                options.walk = false;
                options.moving = false;
                options.random = false;
                for name in value.split(|&byte| byte == b',') {
                    match name {
                        b"addr" => options.addr = true,
                        b"walk" => options.walk = true,
                        b"moving" => options.moving = true,
                        b"random" => options.random = true,
                        _ => return None,
                    }
                }
            }
            _ => return None,
        }
        if key == b"disk" && options.disk.is_none() {
            return None;
        }
    }
    // A soak runs until `for=` or Escape unless told otherwise.
    if options.soak && !options.passes_given {
        options.passes = 0;
    }
    if !options.soak && (options.net.is_some() || options.disk.is_some() || options.for_s != 0) {
        return None; // soak-only options without `soak`
    }
    Some(options)
}

fn parse_switch(value: &[u8]) -> Option<bool> {
    match value {
        b"on" => Some(true),
        b"off" => Some(false),
        _ => None,
    }
}

fn print_hex(putc: Putc, value: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for shift in (0..8).rev() {
        putc(u32::from(DIGITS[(value >> (shift * 4) & 0xf) as usize]));
    }
}

fn print_signed(putc: Putc, value: i64) {
    if value < 0 {
        putc(u32::from(b'-'));
    } else {
        putc(u32::from(b'+'));
    }
    print_u32(putc, value.unsigned_abs().min(u64::from(u32::MAX)) as u32);
}

/// Seconds with one decimal; floats are not available.
fn print_seconds(putc: Putc, micros: u64) {
    let tenths = (micros / 100_000) as u32;
    print_u32(putc, tenths / 10);
    putc(u32::from(b'.'));
    print_u32(putc, tenths % 10);
    print_bytes(putc, b" s");
}

/// `HH:MM:SS`.
fn print_clock(putc: Putc, seconds: u64) {
    let seconds = seconds as u32;
    for (index, part) in [seconds / 3600, seconds / 60 % 60, seconds % 60].into_iter().enumerate() {
        if index != 0 {
            putc(u32::from(b':'));
        }
        putc(u32::from(b'0' + (part / 10 % 10) as u8));
        putc(u32::from(b'0' + (part % 10) as u8));
    }
}

/// Claims blocks from one pool: ask for `want` bytes, on failure halve the
/// request (down to `minimum`), on success keep asking for the same size, until
/// `remaining` bytes are held or the pool has nothing left.
fn claim(
    blocks: &mut [Block; MAX_REGIONS],
    count: &mut usize,
    remaining: &mut u32,
    pool_max: u32,
    granule: u32,
    minimum: u32,
    from_frames: bool,
    alloc: extern "C" fn(u32) -> u32,
) {
    let mut want = (*remaining).min(pool_max) / granule * granule;
    while want >= minimum && *count < MAX_REGIONS && *remaining >= granule {
        want = want.min(*remaining / granule * granule);
        let address = alloc(want);
        if address == 0 {
            want = want / 2 / granule * granule;
            continue;
        }
        blocks[*count] = Block { address, bytes: want, from_frames };
        *count += 1;
        *remaining -= want;
    }
}

/// The kernel's two memory pools, as the tape sees them.
#[derive(Clone, Copy)]
struct Pools {
    alloc: extern "C" fn(u32) -> u32,
    free: extern "C" fn(u32, u32),
    alloc_frames: extern "C" fn(u32) -> u32,
    free_frames: extern "C" fn(u32, u32),
}

impl Pools {
    fn release(&self, block: Block) {
        if block.from_frames {
            (self.free_frames)(block.address, block.bytes);
        } else {
            (self.free)(block.address, block.bytes);
        }
    }

    /// Claims test memory (frame pool first, then the big pool); returns the
    /// number of blocks and the bytes held.
    fn claim_all(&self, options: &Options<'_>, blocks: &mut [Block; MAX_REGIONS]) -> (usize, u64) {
        let mut count = 0;
        let mut remaining = options.size_mib.map_or(u32::MAX, |mib| mib.saturating_mul(MIB));
        if options.frames {
            claim(blocks, &mut count, &mut remaining, FRAME_POOL_MAX, 4096, MIN_FRAME_BLOCK, true, self.alloc_frames);
        }
        claim(blocks, &mut count, &mut remaining, BIG_POOL_MAX, MIB, MIB, false, self.alloc);
        let total = blocks[..count].iter().map(|block| u64::from(block.bytes)).sum();
        (count, total)
    }

    fn release_all(&self, blocks: &[Block]) {
        for &block in blocks {
            self.release(block);
        }
    }
}

fn region_of(block: &Block) -> Region {
    unsafe { Region::new(block.address as *mut u32, (block.bytes / 4) as usize, block.address) }
}

/// Soak's allocator churn: with every test block returned, allocate a batch of
/// random-sized blocks from both pools, fill each with its own random stream,
/// verify them in a rotated order and free them in reverse. Returns `false` if
/// aborted; `refused` counts requests the pools could not satisfy (not errors).
fn churn_round(
    pools: &Pools,
    options: &Options<'_>,
    rng: &mut Rng,
    errors: &mut Errors,
    control: &mut Kernel<'_>,
    refused: &mut u32,
) -> bool {
    let mut held = [NO_BLOCK; CHURN_SLOTS];
    let mut seeds = [0u32; CHURN_SLOTS];
    let mut count = 0;
    let mut finished = true;
    for _ in 0..CHURN_SLOTS {
        let from_frames = options.frames && rng.below(2) == 0;
        let (bytes, address) = if from_frames {
            let bytes = (16 + rng.below(2032)) * 4096; // 64 KiB .. 8 MiB
            (bytes, (pools.alloc_frames)(bytes))
        } else {
            let bytes = (1 + rng.below(8)) * MIB;
            (bytes, (pools.alloc)(bytes))
        };
        if address == 0 {
            *refused += 1;
            continue;
        }
        held[count] = Block { address, bytes, from_frames };
        seeds[count] = rng.next();
        count += 1;
        if !stream_fill(&region_of(&held[count - 1]), control, seeds[count - 1]) {
            finished = false;
            break;
        }
    }
    if finished && count > 0 {
        let start = rng.below(count as u32) as usize;
        for step in 0..count {
            let index = (start + step) % count;
            if !stream_verify(&region_of(&held[index]), errors, control, Test::Churn, seeds[index]) {
                finished = false;
                break;
            }
        }
    }
    for index in (0..count).rev() {
        pools.release(held[index]);
    }
    finished
}

/// The kernel counters soak compares across the run.
struct Baseline {
    free_frames: u32,
    free_big: u32,
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(
    putc: Putc,
    should_abort: extern "C" fn() -> u32,
    alloc: extern "C" fn(u32) -> u32,
    free: extern "C" fn(u32, u32),
    _read_key: extern "C" fn() -> u32,
    args_ptr: u32,
    args_len: u32,
    _nic_mac: extern "C" fn(u32) -> u32,
    _nic_send: extern "C" fn(u32, u32) -> u32,
    _nic_receive: extern "C" fn(u32, u32) -> u32,
    nic_max_frame_len: extern "C" fn() -> u32,
    ticks: extern "C" fn() -> u64,
    nic_count: extern "C" fn() -> u32,
    nic_mac_at: extern "C" fn(u32, u32) -> u32,
    nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    _fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
    fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    fs_remove: extern "C" fn(u32, u32) -> u32,
    _audio_sample_rate: extern "C" fn() -> u32,
    _audio_channels_and_bits: extern "C" fn() -> u32,
    _audio_submit: extern "C" fn(u32, u32) -> u32,
    _audio_capture: extern "C" fn(u32, u32) -> u32,
    _audio_count: extern "C" fn() -> u32,
    _audio_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_channels_and_bits_at: extern "C" fn(u32) -> u32,
    _audio_submit_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_at: extern "C" fn(u32, u32, u32) -> u32,
    _audio_capture_sample_rate: extern "C" fn() -> u32,
    _audio_capture_channels_and_bits: extern "C" fn() -> u32,
    _audio_capture_sample_rate_at: extern "C" fn(u32) -> u32,
    _audio_capture_channels_and_bits_at: extern "C" fn(u32) -> u32,
    _wait_for_event: extern "C" fn(),
    _set_input_mode: extern "C" fn(u32),
    micros: extern "C" fn() -> u64,
    alloc_frames: extern "C" fn(u32) -> u32,
    free_frames: extern "C" fn(u32, u32),
    kernel_stat: extern "C" fn(u32, u32) -> u32,
) {
    let arguments = if args_len == 0 {
        &[][..]
    } else {
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, args_len as usize) }
    };
    let Some(options) = parse_options(arguments) else {
        print_bytes(putc, b"memtest: cannot use '");
        print_bytes(putc, arguments);
        print_bytes(putc, b"'\n");
        print_bytes(
            putc,
            b"Usage: tape run memtest [size=MB] [passes=N] [hold=S] [seed=N]\n\
              \x20      [tests=addr,walk,moving,random] [frames=on|off] [inject=WORD]\n\
              passes=0 repeats until Escape; hold=S adds a retention test.\n\
              Soak: tape run memtest soak [for=90|5m|2h] [status=S] [net=A,B]\n\
              \x20      [net.fps=N] [churn=on|off] [disk=soakNAME [disk.kb=N] [disk.every=S]]\n\
              disk= is never on by itself; the name must start with 'soak'.\n",
        );
        return;
    };
    let env = Env {
        putc,
        micros,
        ticks,
        kernel_stat,
        nic_count,
        nic_mac_at,
        nic_send_at,
        nic_receive_at,
        nic_max_frame_len,
        fs_write,
        fs_read,
        fs_remove,
    };
    let pools = Pools { alloc, free, alloc_frames, free_frames };

    let baseline = Baseline {
        free_frames: kernel_stat(STAT_FREE_FRAMES, 0),
        free_big: kernel_stat(STAT_FREE_BIG, 0),
    };

    // Set up the soak services first: a bad `net=` should not cost a claim.
    let started = micros();
    let mut soak = None;
    if options.soak {
        let Some(built) = build_soak(&env, &options, started) else { return };
        soak = Some(built);
    }

    // Claim memory: the frame pool first (it covers the low 128 MiB, which the
    // big pool cannot), then the big pool.
    let mut blocks = [NO_BLOCK; MAX_REGIONS];
    let (mut count, claimed_bytes) = pools.claim_all(&options, &mut blocks);
    if count == 0 {
        print_bytes(putc, b"memtest: no free memory could be claimed (try frames=on; the big pool\n");
        print_bytes(putc, b"only exists above 128 MiB).\n");
        return;
    }
    print_bytes(putc, if options.soak { b"claimed" } else { b"fos memtest: claimed" });
    for (index, block) in blocks[..count].iter().enumerate() {
        print_bytes(putc, b"\nblock ");
        print_u32(putc, index as u32 + 1);
        print_bytes(putc, b": 0x");
        print_hex(putc, block.address);
        print_bytes(putc, b" + ");
        print_u32(putc, block.bytes / 1024);
        print_bytes(putc, if block.from_frames { b" KiB (frame pool)" } else { b" KiB (big pool)" });
    }
    print_bytes(putc, b"\ntesting ");
    print_u32(putc, (claimed_bytes / u64::from(MIB)) as u32);
    print_bytes(putc, b" MiB; Escape stops. Not covered: the tape's own 4 MiB, the kernel's\n");
    print_bytes(putc, b"low memory, the kernel's 4 MiB frame reserve, and RAM above 2 GiB.\n");
    if let Some(soak) = soak.as_mut() {
        // The clock starts when the tests do, not while memory was being claimed.
        let now = micros();
        soak.started_us = now;
        soak.end_us = if options.for_s == 0 { 0 } else { now + u64::from(options.for_s) * 1_000_000 };
        soak.next_status_us = now + soak.status_every_us;
        soak.next_clock_us = now;
        if let Some(net) = soak.net.as_mut() {
            net.started_us = now;
        }
        if let Some(disk) = soak.disk.as_mut() {
            disk.next_us = now + disk.every_us;
        }
    }

    let mut control = Kernel { should_abort, inject: options.inject, aborted: false, timed_out: false, soak };
    let mut errors = Errors::new();
    let mut rng = Rng::new(options.seed ^ micros() as u32);
    let started = micros();
    let mut printed = 0;
    let mut pass = 0u32;
    let mut churn_refused = 0u32;
    let mut claim_shortfall = 0u64;
    'passes: while options.passes == 0 || pass < options.passes {
        pass += 1;
        let pass_started = micros();
        let pass_errors = errors.total;
        if let Some(soak) = control.soak.as_mut() {
            soak.pass = pass;
        }
        for test in [Test::Address, Test::Walking, Test::Moving, Test::Random, Test::Retention] {
            let enabled = match test {
                Test::Address => options.addr,
                Test::Walking => options.walk,
                Test::Moving => options.moving,
                Test::Random => options.random,
                Test::Retention => options.hold_s > 0,
                Test::Churn => false,
            };
            if !enabled {
                continue;
            }
            let test_started = micros();
            let before = errors.total;
            // A soak runs for hours: its per-test "ok" lines would bury the
            // status lines, so only trouble is printed there.
            if !options.soak {
                print_test_prefix(putc, pass, test.name());
            }
            for block in &blocks[..count] {
                let region = region_of(block);
                let finished = match test {
                    Test::Address => address_test(&region, &mut errors, &mut control),
                    Test::Walking => walking_test(&region, &mut errors, &mut control),
                    Test::Moving => moving_test(&region, &mut errors, &mut control),
                    Test::Random => random_test(&region, &mut errors, &mut control, options.seed.wrapping_add(pass)),
                    Test::Retention => retention_test(&region, &mut errors, &mut control, options.hold_s, micros),
                    Test::Churn => true,
                };
                if !finished {
                    break;
                }
            }
            if !options.soak || errors.total != before {
                if options.soak {
                    print_test_prefix(putc, pass, test.name());
                }
                report_test(putc, &errors, before, test_started, micros, &control);
                print_new_mismatches(putc, &errors, &mut printed);
            }
            if let Some(soak) = control.soak.as_mut() {
                soak.mem_errors = errors.total;
            }
            if control.aborted || control.timed_out {
                break 'passes;
            }
        }
        if !options.soak {
            print_bytes(putc, b"pass ");
            print_u32(putc, pass);
            print_bytes(putc, b" done in ");
            print_seconds(putc, micros() - pass_started);
            print_bytes(putc, b", ");
            print_u32(putc, errors.total - pass_errors);
            print_bytes(putc, b" errors\n");
        }

        if options.soak && options.churn {
            // Hand everything back, churn the allocator, claim it all again.
            pools.release_all(&blocks[..count]);
            let before = errors.total;
            let finished = churn_round(&pools, &options, &mut rng, &mut errors, &mut control, &mut churn_refused);
            if errors.total != before {
                print_test_prefix(putc, pass, b"churn");
                report_test(putc, &errors, before, micros(), micros, &control);
                print_new_mismatches(putc, &errors, &mut printed);
            }
            blocks = [NO_BLOCK; MAX_REGIONS];
            let (again, held_again) = pools.claim_all(&options, &mut blocks);
            count = again;
            if held_again < claimed_bytes {
                claim_shortfall = claimed_bytes - held_again;
                print_bytes(putc, b"  only ");
                print_u32(putc, (held_again / 1024) as u32);
                print_bytes(putc, b" KiB could be claimed again (was ");
                print_u32(putc, (claimed_bytes / 1024) as u32);
                print_bytes(putc, b")\n");
            }
            if !finished || count == 0 {
                break 'passes;
            }
        }
    }

    pools.release_all(&blocks[..count]);
    let finish_us = micros();
    let verdict = Verdict::collect(&control, &errors, claim_shortfall, &baseline, &env);

    // The verdict, with the evidence a failing module needs: which bits and
    // which megabytes.
    let stopped = control.aborted;
    print_bytes(putc, if stopped { b"memtest stopped: " } else { b"memtest finished: " });
    print_u32(putc, pass);
    print_bytes(putc, b" pass(es), ");
    print_u32(putc, (errors.bytes_checked / u64::from(MIB)) as u32);
    print_bytes(putc, b" MiB compared in ");
    print_seconds(putc, finish_us - started);
    print_bytes(putc, b"\n");
    if errors.total != 0 {
        print_bytes(putc, b"memory errors: ");
        print_u32(putc, errors.total);
        print_bytes(putc, b"\nbit flips:");
        for (bit, &flips) in errors.bit_flips.iter().enumerate() {
            if flips != 0 {
                print_bytes(putc, b" bit");
                print_u32(putc, bit as u32);
                print_bytes(putc, b"=");
                print_u32(putc, flips);
            }
        }
        print_bytes(putc, b"\nbad MiB blocks:");
        for &(block, flips) in &errors.bad_blocks[..errors.bad_block_count] {
            print_bytes(putc, b" 0x");
            print_hex(putc, block << 20);
            print_bytes(putc, b"=");
            print_u32(putc, flips);
        }
        print_bytes(putc, b"\n");
    }
    if let Some(soak) = control.soak.as_mut() {
        print_soak_summary(putc, soak, churn_refused, &baseline, &env);
        if let Some(disk) = soak.disk.as_ref() {
            // Leave no scratch file behind.
            (env.fs_remove)(disk.name.as_ptr() as u32, disk.name.len() as u32);
        }
    }
    print_bytes(putc, b"RESULT: ");
    if verdict.reasons == 0 {
        print_bytes(putc, b"PASS, no errors\n");
    } else {
        print_bytes(putc, b"FAIL\n");
        verdict.print(putc);
    }
}

fn report_test(
    putc: Putc,
    errors: &Errors,
    before: u32,
    started_us: u64,
    micros: extern "C" fn() -> u64,
    control: &Kernel<'_>,
) {
    if errors.total == before {
        print_bytes(putc, if control.aborted || control.timed_out { b"stopped" } else { b"ok" });
    } else {
        print_u32(putc, errors.total - before);
        print_bytes(putc, b" ERRORS");
    }
    print_bytes(putc, b" (");
    print_seconds(putc, micros() - started_us);
    print_bytes(putc, b")\n");
}

fn print_test_prefix(putc: Putc, pass: u32, name: &[u8]) {
    print_bytes(putc, b"pass ");
    print_u32(putc, pass);
    print_bytes(putc, b" ");
    print_bytes(putc, name);
    print_bytes(putc, b" ... ");
}

fn print_new_mismatches(putc: Putc, errors: &Errors, printed: &mut usize) {
    while *printed < errors.recorded_count {
        let mismatch = errors.recorded[*printed];
        print_bytes(putc, b"  0x");
        print_hex(putc, mismatch.address);
        print_bytes(putc, b" want ");
        print_hex(putc, mismatch.expected);
        print_bytes(putc, b" got ");
        print_hex(putc, mismatch.actual);
        print_bytes(putc, b" (");
        print_bytes(putc, mismatch.test.name());
        print_bytes(putc, b")\n");
        *printed += 1;
    }
}

/// Validates the soak options against the machine and builds the services, or
/// explains why not (and returns `None`).
fn build_soak<'a>(env: &Env, options: &Options<'a>, now: u64) -> Option<Soak<'a>> {
    let putc = env.putc;
    let mut net = None;
    if let Some((first, second)) = options.net {
        let total = (env.nic_count)();
        if first == second || first >= total || second >= total {
            print_bytes(putc, b"memtest: net=A,B needs two different NICs from 'nic list'.\n");
            return None;
        }
        let mut mac = [[0u8; 6]; 2];
        for (slot, nic) in [first, second].into_iter().enumerate() {
            if (env.nic_mac_at)(nic, mac[slot].as_mut_ptr() as u32) == 0 {
                print_bytes(putc, b"memtest: cannot read a NIC's MAC address.\n");
                return None;
            }
        }
        let stat = |id, nic| (env.kernel_stat)(id, nic);
        net = Some(NetPair {
            nic: [first, second],
            mac,
            fps: options.net_fps,
            max_frame: ((env.nic_max_frame_len)() as usize).clamp(MIN_FRAME, FRAME_BUFFER),
            started_us: now,
            stats: [LinkStats::default(); 2],
            sequence: [0; 2],
            recoveries_at_start: [stat(STAT_NIC_RECOVERIES, first), stat(STAT_NIC_RECOVERIES, second)],
            overruns_at_start: [stat(STAT_NIC_OVERRUNS, first), stat(STAT_NIC_OVERRUNS, second)],
        });
    }
    let disk = options.disk.map(|name| DiskCheck {
        name,
        bytes: options.disk_kb * 1024,
        every_us: u64::from(options.disk_every_s) * 1_000_000,
        next_us: now,
        rounds: 0,
        ok: 0,
        mismatches: 0,
        errors: 0,
        last_error: 0,
        written: 0,
    });
    print_bytes(putc, b"fos memtest soak: ");
    match options.for_s {
        0 => print_bytes(putc, b"until Escape"),
        seconds => {
            print_clock(putc, u64::from(seconds));
            print_bytes(putc, b" (hh:mm:ss)");
        }
    }
    print_bytes(putc, b"; status every ");
    print_u32(putc, options.status_s);
    print_bytes(putc, b" s\n  net: ");
    match &net {
        Some(pair) => {
            print_bytes(putc, b"NIC ");
            print_u32(putc, pair.nic[0] + 1);
            print_bytes(putc, b" <-> NIC ");
            print_u32(putc, pair.nic[1] + 1);
            print_bytes(putc, b", ");
            print_u32(putc, pair.fps);
            print_bytes(putc, b" frames/s each way\n");
        }
        None => print_bytes(putc, b"off (net=A,B for two cabled NICs)\n"),
    }
    print_bytes(putc, b"  churn: ");
    print_bytes(putc, if options.churn { b"on\n" } else { b"off\n" });
    print_bytes(putc, b"  disk: ");
    match &disk {
        Some(check) => {
            print_bytes(putc, b"WRITES ");
            print_u32(putc, check.bytes / 1024);
            print_bytes(putc, b" KiB to '");
            print_bytes(putc, check.name);
            print_bytes(putc, b"' every ");
            print_u32(putc, options.disk_every_s);
            print_bytes(putc, b" s (flash wear)\n");
        }
        None => print_bytes(putc, b"off, never automatic (disk=soakNAME opts in)\n"),
    }
    Some(Soak {
        env: *env,
        started_us: now,
        end_us: 0,
        status_every_us: u64::from(options.status_s) * 1_000_000,
        next_status_us: now,
        next_clock_us: now,
        clock: ClockMonitor::new(),
        net,
        disk,
        pass: 0,
        mem_errors: 0,
    })
}

fn print_soak_summary(putc: Putc, soak: &mut Soak<'_>, churn_refused: u32, baseline: &Baseline, env: &Env) {
    // One last service so the final counters include the last frames in flight.
    let now = (env.micros)();
    soak.clock.sample(now, (env.ticks)());
    if soak.net.is_some() {
        for _ in 0..50 {
            soak.service_net(now);
        }
    }
    print_bytes(putc, b"soak summary after ");
    print_clock(putc, (now - soak.started_us) / 1_000_000);
    print_bytes(putc, b":\n  clock: skew ");
    print_signed(putc, soak.clock.skew_us);
    print_bytes(putc, b" us (max ");
    print_u32(putc, soak.clock.max_skew_us.min(u64::from(u32::MAX)) as u32);
    print_bytes(putc, b"), drift ");
    print_signed(putc, soak.clock.drift_ppm());
    print_bytes(putc, b" ppm, backwards ");
    print_u32(putc, soak.clock.backwards);
    print_bytes(putc, b", tick stalls ");
    print_u32(putc, soak.clock.stalls);
    print_bytes(putc, b"\n  frames: ");
    print_u32(putc, baseline.free_frames);
    print_bytes(putc, b" free at start, ");
    print_u32(putc, (env.kernel_stat)(STAT_FREE_FRAMES, 0));
    print_bytes(putc, b" now\n  big pool: ");
    print_u32(putc, baseline.free_big);
    print_bytes(putc, b" -> ");
    print_u32(putc, (env.kernel_stat)(STAT_FREE_BIG, 0));
    print_bytes(putc, b" MiB free; churn refused ");
    print_u32(putc, churn_refused);
    print_bytes(putc, b"\n");
    if let Some(net) = &soak.net {
        for direction in 0..2 {
            let stats = &net.stats[direction];
            print_bytes(putc, b"  net NIC ");
            print_u32(putc, net.nic[direction] + 1);
            print_bytes(putc, b" -> ");
            print_u32(putc, net.nic[1 - direction] + 1);
            print_bytes(putc, b": sent ");
            print_u32(putc, stats.sent);
            print_bytes(putc, b" rx ");
            print_u32(putc, stats.received);
            print_bytes(putc, b" missing ");
            print_u32(putc, stats.missing());
            print_bytes(putc, b" bad ");
            print_u32(putc, stats.corrupt);
            print_bytes(putc, b" late ");
            print_u32(putc, stats.out_of_order);
            print_bytes(putc, b" txfail ");
            print_u32(putc, stats.send_failures);
            print_bytes(putc, b"\n");
        }
        for (slot, &nic) in net.nic.iter().enumerate() {
            let recoveries = (env.kernel_stat)(STAT_NIC_RECOVERIES, nic).wrapping_sub(net.recoveries_at_start[slot]);
            let overruns = (env.kernel_stat)(STAT_NIC_OVERRUNS, nic).wrapping_sub(net.overruns_at_start[slot]);
            print_bytes(putc, b"  NIC ");
            print_u32(putc, nic + 1);
            print_bytes(putc, b": watchdog recoveries ");
            print_u32(putc, recoveries);
            print_bytes(putc, b", rx overruns ");
            print_u32(putc, overruns);
            print_bytes(putc, b"\n");
        }
    }
    if let Some(disk) = &soak.disk {
        print_bytes(putc, b"  disk: ");
        print_u32(putc, disk.rounds);
        print_bytes(putc, b" rounds, ok ");
        print_u32(putc, disk.ok);
        print_bytes(putc, b" mismatches ");
        print_u32(putc, disk.mismatches);
        print_bytes(putc, b" errors ");
        print_u32(putc, disk.errors);
        print_bytes(putc, b", ");
        print_u32(putc, (disk.written / 1024) as u32);
        print_bytes(putc, b" KiB written\n");
    }
}

/// Everything that makes a run fail, as one bit per reason.
struct Verdict {
    reasons: u32,
}

const REASONS: [(u32, &[u8]); 11] = [
    (1 << 0, b"memory errors"),
    (1 << 1, b"claimed memory did not all come back after churn"),
    (1 << 2, b"free frames or big-pool chunks differ from the start (leak)"),
    (1 << 3, b"no soak frame ever arrived (cable? net=A,B must be cabled together)"),
    (1 << 4, b"soak frames lost, corrupted or duplicated"),
    (1 << 5, b"clock ran backwards"),
    (1 << 6, b"timer ticks stopped while microseconds advanced"),
    (1 << 7, b"disk file read back wrong or filesystem errors"),
    (1 << 8, b"a NIC needed a watchdog recovery"),
    (1 << 9, b"disk=: nothing was ever written (run too short for disk.every)"),
    (1 << 10, b"net: frames could not be transmitted"),
];

impl Verdict {
    fn collect(control: &Kernel<'_>, errors: &Errors, shortfall: u64, baseline: &Baseline, env: &Env) -> Verdict {
        let mut reasons = 0;
        if errors.total != 0 {
            reasons |= 1 << 0;
        }
        if shortfall != 0 {
            reasons |= 1 << 1;
        }
        let Some(soak) = control.soak.as_ref() else {
            return Verdict { reasons };
        };
        if (env.kernel_stat)(STAT_FREE_FRAMES, 0) != baseline.free_frames
            || (env.kernel_stat)(STAT_FREE_BIG, 0) != baseline.free_big
        {
            reasons |= 1 << 2;
        }
        if let Some(net) = &soak.net {
            let received = net.stats[0].received + net.stats[1].received;
            let sent = net.stats[0].sent + net.stats[1].sent;
            if sent != 0 && received == 0 {
                reasons |= 1 << 3;
            } else if net.stats.iter().any(|link| link.missing() != 0 || link.corrupt != 0 || link.out_of_order != 0) {
                reasons |= 1 << 4;
            }
            if net.stats.iter().any(|link| link.send_failures != 0) {
                reasons |= 1 << 10;
            }
            for (slot, &nic) in net.nic.iter().enumerate() {
                if (env.kernel_stat)(STAT_NIC_RECOVERIES, nic).wrapping_sub(net.recoveries_at_start[slot]) != 0 {
                    reasons |= 1 << 8;
                }
            }
        }
        if soak.clock.backwards != 0 {
            reasons |= 1 << 5;
        }
        if soak.clock.stalls != 0 {
            reasons |= 1 << 6;
        }
        if let Some(disk) = &soak.disk {
            if disk.mismatches != 0 || disk.errors != 0 {
                reasons |= 1 << 7;
            }
            if disk.rounds == 0 {
                reasons |= 1 << 9;
            }
        }
        Verdict { reasons }
    }

    fn print(&self, putc: Putc) {
        for (bit, text) in REASONS {
            if self.reasons & bit != 0 {
                print_bytes(putc, b"  - ");
                print_bytes(putc, text);
                print_bytes(putc, b"\n");
            }
        }
    }
}

/// Retention: fill with a pattern, leave it alone for `seconds`, verify; then
/// the complement. A cell that leaks charge (or a refresh that is too slow)
/// only shows up when the data has sat idle.
fn retention_test(
    region: &Region,
    errors: &mut Errors,
    control: &mut Kernel<'_>,
    seconds: u32,
    micros: extern "C" fn() -> u64,
) -> bool {
    for pattern in [0xaaaa_aaaa, 0x5555_5555] {
        if !fill_pattern(region, control, pattern) {
            return false;
        }
        let deadline = micros() + u64::from(seconds) * 1_000_000;
        while micros() < deadline {
            if !control.poll() {
                return false;
            }
        }
        if !verify_pattern(region, errors, control, Test::Retention, pattern) {
            return false;
        }
    }
    true
}
