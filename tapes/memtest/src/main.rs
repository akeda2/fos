//! `memtest`: a RAM tester and long-run soak test, as a v2 tape.
//!
//! ```text
//! tape run memtest [size=MB] [passes=N] [tests=addr,walk,moving,random] [hold=SECONDS]
//!                  [seed=N] [inject=WORD] [chunk=MB]
//! tape run memtest soak [for=90|5m|2h] [status=S] [net=A,B] [net.fps=N] [churn=on|off]
//!                  [disk=soakNAME [disk.kb=N] [disk.every=S]]  ... plus any option above
//! ```
//!
//! **Memory mode** claims free RAM from the kernel (`alloc`, in `chunk` MiB pieces,
//! halving when a piece is refused) and runs the selected tests over every claimed
//! block, reporting mismatches with their physical addresses. `size=` caps the total in
//! MiB (default: everything the memory map shows free, less a 64 MiB reserve for the
//! kernel and the tape itself); `passes=0` repeats until Escape. A 64-bit tape addresses
//! all of RAM directly, so there are no pools and no mode switches here (v1 needed both).
//!
//! **Soak mode** is "how does the OS hold up under long, heavy use": the memory tests
//! repeat until `for=` elapses (or Escape), and between passes it
//!  * churns the allocator (`churn`, default on): frees everything, allocates random-sized
//!    blocks, fills each with its own random stream, checks and frees in another order
//!    (overlapping allocations show as corrupt data), then claims the memory again;
//!  * compares `micros()` with the 100 Hz tick (drift, lost timer interrupts);
//!  * compares the kernel's free-page count before and after (leaks);
//!  * with `net=A,B` (two NICs cabled to each other, one-based as `nic` lists them; keep
//!    `term net` on another NIC) each sends checksummed frames of varying length to the other
//!    at `net.fps` frames/s (default 1000) and counts lost, corrupt, duplicated and
//!    reordered ones;
//!  * with `disk=NAME` (never on by itself: the machine may boot from a USB stick, and a
//!    disk test writes), writes a pseudo-random file through FOSFS every `disk.every`
//!    seconds, reads it back and compares. The name must start with `soak`, so a real file
//!    can never be overwritten; it is removed at the end.
//!
//! A status line prints every `status=` seconds and the run ends with PASS/FAIL and every
//! reason. The test engine is `engine.rs`.
#![no_std]
#![no_main]

mod engine;

use engine::*;
use tape_rt::{TapeApi, print, println};

const MAX_BLOCKS: usize = 512;
const RESERVE_BYTES: u64 = 64 << 20;
const MIB: u64 = 1 << 20;

#[derive(Clone, Copy)]
struct Block {
    address: usize,
    bytes: usize,
}

struct Options {
    size_mb: Option<u64>,
    passes: u32,
    addr: bool,
    walk: bool,
    moving: bool,
    random: bool,
    hold: u32,
    seed: u32,
    inject: Option<usize>,
    chunk_mb: u64,
    soak: bool,
    soak_seconds: u64,
    status_seconds: u64,
    churn: bool,
    net: Option<(usize, usize)>,
    net_fps: u64,
    disk: Option<[u8; 16]>,
    disk_length: usize,
    disk_kb: usize,
    disk_every: u64,
}

fn parse_duration(text: &str) -> Option<u64> {
    let (number, multiplier) = match text.as_bytes().last()? {
        b'm' => (&text[..text.len() - 1], 60),
        b'h' => (&text[..text.len() - 1], 3600),
        b's' => (&text[..text.len() - 1], 1),
        _ => (text, 1),
    };
    number.parse::<u64>().ok().map(|n| n * multiplier)
}

fn parse(arguments: &str) -> Result<Options, &str> {
    let mut options = Options {
        size_mb: None, passes: 1, addr: true, walk: true, moving: true, random: true, hold: 0, seed: 1, inject: None,
        chunk_mb: 64, soak: false, soak_seconds: 90, status_seconds: 10, churn: true, net: None, net_fps: 1000, disk: None, disk_length: 0, disk_kb: 32, disk_every: 120,
    };
    for word in arguments.split_ascii_whitespace() {
        let (key, value) = word.split_once('=').unwrap_or((word, ""));
        let number = || value.parse::<u64>().map_err(|_| word);
        match key {
            "soak" => {
                options.soak = true;
                options.passes = 0;
            }
            "size" => options.size_mb = Some(number()?),
            "passes" => options.passes = number()? as u32,
            "hold" => options.hold = number()? as u32,
            "seed" => options.seed = number()? as u32,
            "inject" => options.inject = Some(number()? as usize),
            "chunk" => options.chunk_mb = number()?.max(1),
            "for" => options.soak_seconds = parse_duration(value).ok_or(word)?,
            "status" => options.status_seconds = number()?.max(1),
            "churn" => options.churn = value != "off",
            "net" => {
                let (a, b) = value.split_once(',').ok_or(word)?;
                let (a, b) = (a.parse::<usize>().map_err(|_| word)?, b.parse::<usize>().map_err(|_| word)?);
                if a == 0 || b == 0 || a == b {
                    return Err("net=A,B needs two different NICs numbered from 1");
                }
                options.net = Some((a - 1, b - 1));
            }
            "net.fps" => options.net_fps = number()?.clamp(1, 20_000),
            "disk" => {
                if !value.starts_with("soak") || value.len() > 16 {
                    return Err("disk= must name a file starting with `soak` (up to 16 characters)");
                }
                let mut name = [0u8; 16];
                name[..value.len()].copy_from_slice(value.as_bytes());
                options.disk = Some(name);
                options.disk_length = value.len();
            }
            "disk.kb" => options.disk_kb = number()?.clamp(1, 1024) as usize,
            "disk.every" => options.disk_every = number()?.max(1),
            "tests" => {
                (options.addr, options.walk, options.moving, options.random) = (false, false, false, false);
                for name in value.split(',') {
                    match name {
                        "addr" => options.addr = true,
                        "walk" => options.walk = true,
                        "moving" => options.moving = true,
                        "random" => options.random = true,
                        _ => return Err(word),
                    }
                }
            }
            _ => return Err(word),
        }
    }
    Ok(options)
}

const FRAME_BUFFER: usize = 1536;
static mut TX_FRAME: [u8; FRAME_BUFFER] = [0; FRAME_BUFFER];
static mut RX_FRAME: [u8; FRAME_BUFFER] = [0; FRAME_BUFFER];

/// Two cabled NICs exchanging checksummed frames, one stream per direction.
struct NetPair {
    nic: [usize; 2],
    mac: [[u8; 6]; 2],
    fps: u64,
    max_frame: usize,
    started: u64,
    /// `stats[d]`: the stream sent by `nic[d]` and received by `nic[1 - d]`.
    stats: [LinkStats; 2],
    sequence: [u32; 2],
}

impl NetPair {
    /// Sends what is due (at most 16 frames per direction per call) and drains both
    /// receive rings. Called from the engine's poll hook, so it must stay cheap when idle.
    fn service(&mut self, api: &TapeApi, now: u64) {
        let tx = unsafe { &mut *core::ptr::addr_of_mut!(TX_FRAME) };
        let rx = unsafe { &mut *core::ptr::addr_of_mut!(RX_FRAME) };
        let due = (now - self.started) * self.fps / 1_000_000;
        for direction in 0..2 {
            let mut burst = 0;
            while u64::from(self.sequence[direction]) < due && burst < 16 {
                let sequence = self.sequence[direction];
                // A pseudo-random length from 60 bytes up to the NIC's limit.
                let span = self.max_frame - MIN_FRAME + 1;
                let length = MIN_FRAME + (sequence.wrapping_mul(2_654_435_761) >> 8) as usize % span;
                let built = build_frame(tx, self.mac[1 - direction], self.mac[direction], sequence, length);
                if (api.nic_send)(self.nic[direction], tx.as_ptr(), built) == 0 {
                    self.stats[direction].send_failures += 1;
                    break; // try again later, with the same sequence number
                }
                self.stats[direction].sent += 1;
                self.sequence[direction] += 1;
                burst += 1;
            }
        }
        for receiver in 0..2 {
            for _ in 0..16 {
                let length = (api.nic_receive)(self.nic[receiver], rx.as_mut_ptr(), self.max_frame);
                if length < 14 {
                    break;
                }
                let frame = &rx[..length.min(FRAME_BUFFER)];
                let sender = 1 - receiver;
                if frame[6..12] != self.mac[sender] {
                    continue; // somebody else's traffic
                }
                match check_frame(frame) {
                    FrameCheck::Good(sequence) => self.stats[sender].on_good(sequence),
                    FrameCheck::Corrupt => self.stats[sender].corrupt += 1,
                    FrameCheck::NotOurs => {}
                }
            }
        }
    }

    fn sent(&self) -> u32 {
        self.stats[0].sent + self.stats[1].sent
    }
    fn received(&self) -> u32 {
        self.stats[0].received + self.stats[1].received
    }
}

/// What the engine's `poll` hook drives: abort, fault injection, the soak scheduler.
struct Controller<'a> {
    api: &'a TapeApi,
    abort: bool,
    inject: Option<usize>,
    injected: bool,
    soak: bool,
    clock: ClockMonitor,
    net: Option<NetPair>,
    next_sample: u64,
    next_status: u64,
    status_every: u64,
    started: u64,
    errors_seen: u32,
    passes_done: u32,
}

impl Control for Controller<'_> {
    fn poll(&mut self) -> bool {
        if (self.api.should_abort)() != 0 {
            self.abort = true;
            return false;
        }
        let now = tape_rt::micros();
        if let Some(net) = self.net.as_mut() {
            net.service(self.api, now);
        }
        if now >= self.next_sample {
            self.clock.sample(now, tape_rt::ticks());
            self.next_sample = now + 500_000;
        }
        if self.soak && now >= self.next_status {
            self.next_status = now + self.status_every * 1_000_000;
            println!(
                "[{:>4} s] pass {}, clock skew {:+} us ({} ppm), {} stalls",
                (now - self.started) / 1_000_000,
                self.passes_done + 1,
                self.clock.skew_us,
                self.clock.drift_ppm(),
                self.clock.stalls
            );
            if let Some(net) = &self.net {
                println!(
                    "         net tx {} rx {} missing {} bad {}",
                    net.sent(),
                    net.received(),
                    net.stats[0].missing() + net.stats[1].missing(),
                    net.stats[0].corrupt + net.stats[1].corrupt
                );
            }
        }
        true
    }

    fn after_write(&mut self, region: &Region) {
        if let (Some(word), false) = (self.inject, self.injected) {
            region.corrupt(word % region.words(), 1 << 5);
            self.injected = true;
        }
    }
}

/// Claims RAM from the kernel: `chunk` MiB pieces, halving on refusal, until `limit` bytes.
fn claim(api: &TapeApi, blocks: &mut [Block; MAX_BLOCKS], count: &mut usize, limit: u64, chunk_mb: u64) -> u64 {
    let mut claimed: u64 = blocks[..*count].iter().map(|b| b.bytes as u64).sum();
    let mut chunk = chunk_mb * MIB;
    while claimed < limit && *count < MAX_BLOCKS && chunk >= MIB {
        let want = chunk.min(limit - claimed);
        let address = (api.alloc)(want as usize);
        if address == 0 {
            chunk /= 2;
            continue;
        }
        blocks[*count] = Block { address, bytes: want as usize };
        *count += 1;
        claimed += want;
    }
    claimed
}

fn release(api: &TapeApi, blocks: &mut [Block; MAX_BLOCKS], count: &mut usize) {
    for block in &blocks[..*count] {
        (api.free)(block.address, block.bytes);
    }
    *count = 0;
}

fn region_of(block: &Block) -> Region {
    unsafe { Region::new(block.address as *mut u32, block.bytes / 4, block.address as u64) }
}

/// Allocator churn: random-sized blocks, each with its own stream, checked in another order.
fn churn(api: &TapeApi, control: &mut Controller, errors: &mut Errors, rng: &mut Rng) -> bool {
    let mut batch = [Block { address: 0, bytes: 0 }; 24];
    let mut made = 0;
    for slot in batch.iter_mut() {
        let bytes = (4096 + rng.below(2 * 1024 * 1024)) as usize & !0xfff;
        let address = (api.alloc)(bytes);
        if address == 0 {
            break;
        }
        *slot = Block { address, bytes };
        made += 1;
    }
    let mut ok = true;
    for (index, block) in batch[..made].iter().enumerate() {
        ok &= stream_fill(&region_of(block), control, 0x1000 + index as u32);
    }
    for index in (0..made).rev() {
        ok &= stream_verify(&region_of(&batch[index]), errors, control, Test::Churn, 0x1000 + index as u32);
    }
    for block in &batch[..made] {
        (api.free)(block.address, block.bytes);
    }
    ok
}

/// Writes a random file through FOSFS, reads it back, compares. Returns false on failure.
fn disk_check(api: &TapeApi, name: &[u8], kb: usize, seed: u32) -> Result<(), &'static str> {
    let bytes = kb * 1024;
    let buffer = (api.alloc)(2 * bytes);
    if buffer == 0 {
        return Err("no memory for the disk buffer");
    }
    let (write, read) = unsafe { (core::slice::from_raw_parts_mut(buffer as *mut u8, bytes), core::slice::from_raw_parts_mut((buffer + bytes) as *mut u8, bytes)) };
    fill_bytes(write, seed);
    let outcome = if (api.fs_write)(name.as_ptr(), name.len(), write.as_ptr(), bytes) != 0 {
        Err("fs_write failed")
    } else if (api.fs_read)(name.as_ptr(), name.len(), read.as_mut_ptr(), bytes) != bytes as isize {
        Err("fs_read returned the wrong length")
    } else if first_mismatch(read, seed).is_some() {
        Err("the file read back differently from what was written")
    } else {
        Ok(())
    };
    (api.free)(buffer, 2 * bytes);
    outcome
}

fn run_tests(options: &Options, blocks: &[Block], control: &mut Controller, errors: &mut Errors, seed: u32) -> bool {
    for block in blocks {
        let region = region_of(block);
        let ok = (!options.addr || address_test(&region, errors, control))
            && (!options.walk || walking_test(&region, errors, control))
            && (!options.moving || moving_test(&region, errors, control))
            && (!options.random || random_test(&region, errors, control, seed));
        if !ok {
            return false;
        }
        if options.hold > 0 && !hold_test(&region, errors, control, options.hold) {
            return false;
        }
    }
    true
}

/// Retention: fill, wait with the memory idle (refresh is the only thing keeping it), verify.
fn hold_test(region: &Region, errors: &mut Errors, control: &mut Controller, seconds: u32) -> bool {
    for pattern in [0xaaaa_aaaau32, 0x5555_5555] {
        if !fill_pattern(region, control, pattern) {
            return false;
        }
        let until = tape_rt::micros() + u64::from(seconds) * 1_000_000;
        while tape_rt::micros() < until {
            if (control.api.should_abort)() != 0 {
                control.abort = true;
                return false;
            }
            tape_rt::idle();
        }
        if !verify_pattern(region, errors, control, Test::Retention, pattern) {
            return false;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let options = match parse(arguments) {
        Ok(options) => options,
        Err(bad) => {
            println!("memtest: cannot understand `{bad}`. See the header of tapes/memtest/src/main.rs.");
            return;
        }
    };

    // How much is free, from the firmware map (type 7 = conventional memory).
    let mut conventional = 0u64;
    for index in 0..api.memory_map_count {
        let entry = unsafe { api.memory_map.add(index * api.memory_map_descriptor_size) };
        let (kind, pages) = unsafe { ((entry as *const u32).read_unaligned(), (entry.add(24) as *const u64).read_unaligned()) };
        if kind == 7 {
            conventional += pages * 4096;
        }
    }
    let usable = conventional.saturating_sub(RESERVE_BYTES);
    let limit = options.size_mb.map_or(usable, |mb| (mb * MIB).min(usable));

    let mut blocks = [Block { address: 0, bytes: 0 }; MAX_BLOCKS];
    let mut count = 0;
    let pages_before = (api.free_pages)();
    let claimed = claim(api, &mut blocks, &mut count, limit, options.chunk_mb);
    println!(
        "memtest: firmware reports {} MiB free; claimed {} MiB in {} block(s) ({} MiB kept back for the kernel).",
        conventional / MIB,
        claimed / MIB,
        count,
        RESERVE_BYTES / MIB
    );
    if claimed == 0 {
        println!("memtest: nothing could be claimed.");
        return;
    }

    // Set up the network pair first-class: a bad `net=` should not run a long test first.
    let mut net = None;
    if let Some((first, second)) = options.net {
        let nics = (api.nic_count)();
        if first >= nics || second >= nics {
            println!("memtest: net=A,B names a NIC this machine does not have ({nics} found; see `nic`).");
            release(api, &mut blocks, &mut count);
            return;
        }
        let mut macs = [[0u8; 6]; 2];
        (api.nic_mac)(first, macs[0].as_mut_ptr());
        (api.nic_mac)(second, macs[1].as_mut_ptr());
        net = Some(NetPair {
            nic: [first, second],
            mac: macs,
            fps: options.net_fps,
            max_frame: (api.nic_max_frame_len)().clamp(MIN_FRAME, FRAME_BUFFER),
            started: 0,
            stats: [LinkStats::default(); 2],
            sequence: [0; 2],
        });
        println!("memtest: network soak between NIC {} and NIC {} at {} frames/s each way.", first + 1, second + 1, options.net_fps);
    }

    let started = tape_rt::micros();
    if let Some(pair) = net.as_mut() {
        pair.started = started;
    }
    let mut control = Controller {
        api, abort: false, inject: options.inject, injected: false, soak: options.soak, clock: ClockMonitor::new(), net,
        next_sample: 0, next_status: started + options.status_seconds * 1_000_000, status_every: options.status_seconds,
        started, errors_seen: 0, passes_done: 0,
    };
    let mut errors = Errors::new();
    let mut rng = Rng::new(options.seed);
    let mut problems = [""; 8];
    let mut problem_count = 0;
    let mut note = |text: &'static str| {
        if problem_count < problems.len() {
            problems[problem_count] = text;
            problem_count += 1;
        }
    };
    let mut next_disk = started + options.disk_every * 1_000_000;
    let mut disk_checks = 0u32;
    let deadline = started + options.soak_seconds * 1_000_000;

    let mut pass = 0u32;
    loop {
        pass += 1;
        control.passes_done = pass - 1;
        let seed = options.seed.wrapping_add(pass.wrapping_mul(0x9e37_79b9));
        if !run_tests(&options, &blocks[..count], &mut control, &mut errors, seed) {
            break;
        }
        if !options.soak && options.passes != 0 {
            println!("pass {pass}/{}: {} error(s) so far, {} MiB verified", options.passes, errors.total, errors.bytes_checked / MIB);
        }
        if options.soak {
            if options.churn {
                release(api, &mut blocks, &mut count);
                if !churn(api, &mut control, &mut errors, &mut rng) {
                    break;
                }
                let again = claim(api, &mut blocks, &mut count, limit, options.chunk_mb);
                if again + 8 * MIB < claimed {
                    note("memory did not come back after churn (allocator leak?)");
                }
            }
            if let Some(name) = options.disk {
                if tape_rt::micros() >= next_disk {
                    next_disk = tape_rt::micros() + options.disk_every * 1_000_000;
                    disk_checks += 1;
                    if let Err(reason) = disk_check(api, &name[..options.disk_length], options.disk_kb, seed) {
                        println!("disk check failed: {reason}");
                        note("a disk write/read-back check failed");
                    }
                }
            }
            if tape_rt::micros() >= deadline {
                break;
            }
        } else if options.passes != 0 && pass >= options.passes {
            break;
        }
    }
    let elapsed = tape_rt::micros() - started;
    if let Some(name) = options.disk {
        let _ = (api.fs_remove)(name.as_ptr(), options.disk_length);
    }
    release(api, &mut blocks, &mut count);

    // ---- report ----
    if control.abort {
        println!("memtest: stopped by the operator.");
    }
    println!("checked {} MiB in {} s ({} passes).", errors.bytes_checked / MIB, elapsed / 1_000_000, pass);
    if errors.total != 0 {
        println!("{} mismatching word(s). First ones:", errors.total);
        for mismatch in &errors.recorded[..errors.recorded_count] {
            println!(
                "  {:#012x}: expected {:#010x}, read {:#010x}, differs in {:#010x} ({})",
                mismatch.address,
                mismatch.expected,
                mismatch.actual,
                mismatch.expected ^ mismatch.actual,
                core::str::from_utf8(mismatch.test.name()).unwrap_or("?")
            );
        }
        print!("  bad bits:");
        for (bit, &flips) in errors.bit_flips.iter().enumerate() {
            if flips != 0 {
                print!(" {bit}({flips})");
            }
        }
        println!();
        for &(block, bad) in &errors.bad_blocks[..errors.bad_block_count] {
            println!("  bad 1 MiB block at {:#x}: {bad} error(s)", block << 20);
        }
    }
    if options.soak {
        println!(
            "clock: {} samples, max skew {} us, {} backwards, {} stalls; disk checks: {}.",
            control.clock.samples, control.clock.max_skew_us, control.clock.backwards, control.clock.stalls, disk_checks
        );
        if control.clock.backwards != 0 {
            note("the microsecond clock went backwards");
        }
        if control.clock.stalls != 0 {
            note("the timer tick stopped while time passed (lost timer interrupts)");
        }
        let pages_after = (api.free_pages)();
        if pages_after + 256 < pages_before {
            note("the kernel's free-page count shrank (memory leak)");
        }
        println!("kernel free pages: {pages_before} before, {pages_after} after.");
        if let Some(net) = control.net.as_mut() {
            // Let the last frames in flight arrive before counting.
            for _ in 0..50 {
                net.service(api, net.started);
            }
            for direction in 0..2 {
                let stats = &net.stats[direction];
                println!(
                    "net NIC {} -> NIC {}: sent {} received {} missing {} corrupt {} duplicate/reordered {} send failures {}",
                    net.nic[direction] + 1,
                    net.nic[1 - direction] + 1,
                    stats.sent,
                    stats.received,
                    stats.missing(),
                    stats.corrupt,
                    stats.out_of_order,
                    stats.send_failures
                );
            }
            if net.sent() != 0 && net.received() == 0 {
                note("no soak frame ever arrived (cable? net=A,B must be cabled together)");
            } else if net.stats.iter().any(|link| link.missing() != 0 || link.corrupt != 0 || link.out_of_order != 0) {
                note("soak frames lost, corrupted or duplicated");
            }
            if net.stats.iter().any(|link| link.send_failures != 0) {
                note("net: frames could not be transmitted");
            }
        }
    }
    if errors.total != 0 {
        note("RAM errors were found");
    }
    if problem_count == 0 && !control.abort {
        println!("memtest: PASS");
    } else if problem_count == 0 {
        println!("memtest: stopped before finishing; no problems seen so far.");
    } else {
        println!("memtest: FAIL");
        for reason in &problems[..problem_count] {
            println!("  - {reason}");
        }
    }
}

tape_rt::tape_help!(br#"memtest: RAM tester and soak test.

  tape run memtest [size=MB] [passes=N] [tests=addr,walk,moving,random]
                   [hold=SECONDS] [seed=N] [inject=WORD] [chunk=MB]
  tape run memtest soak [for=90|5m|2h] [status=S] [net=A,B] [net.fps=N]
                   [churn=on|off] [disk=soakNAME [disk.kb=N] [disk.every=S]]

  Escape stops.
"#);
