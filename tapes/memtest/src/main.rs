//! `memtest`: a RAM tester that runs as a tape.
//!
//! ```text
//! tape run memtest [size=MB] [passes=N] [tests=addr,walk,moving,random]
//!                  [hold=SECONDS] [frames=on|off] [seed=N] [inject=WORD]
//! ```
//!
//! It claims every free block of RAM it can get from the kernel -- the 4 KiB
//! frame pool through `alloc_frames` (ABI slot 39) and the 1 MiB big pool
//! through `alloc` -- up to `size=` MiB in total (default: everything), runs
//! the selected tests over each block and reports mismatches with their
//! physical addresses. `passes=0` repeats until Escape. The kernel keeps a
//! 4 MiB reserve of frames for itself, and the tape's own 4 MiB, the kernel's
//! low-memory region and anything above 2 GiB (unmapped) are out of reach, so
//! "no errors" covers what was claimed, which the tape prints up front.
//!
//! `hold=S` adds a retention test: fill, wait S seconds, verify, then the same
//! with the complement. `inject=WORD` deliberately flips a bit at that word
//! index of every block after each write sweep, to prove the detection works
//! (QEMU's RAM never fails by itself). The engine is in `lib.rs`.
#![no_std]
#![no_main]

extern crate memtest_tape;

use memtest_tape::{
    Control, Errors, Region, Test, address_test, fill_pattern, moving_test, random_test, verify_pattern,
    walking_test,
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

#[derive(Clone, Copy)]
struct Block {
    address: u32,
    bytes: u32,
    from_frames: bool,
}

struct Options {
    size_mib: Option<u32>,
    passes: u32,
    walk: bool,
    addr: bool,
    moving: bool,
    random: bool,
    hold_s: u32,
    frames: bool,
    seed: u32,
    inject: Option<usize>,
}

/// The engine's view of the kernel: abort checks (which also keep the
/// watchdog fed) and the optional fault injection.
struct Kernel {
    should_abort: extern "C" fn() -> u32,
    inject: Option<usize>,
    aborted: bool,
}

impl Control for Kernel {
    fn poll(&mut self) -> bool {
        if (self.should_abort)() != 0 {
            self.aborted = true;
        }
        !self.aborted
    }

    fn after_write(&mut self, region: &Region) {
        if let Some(index) = self.inject {
            region.corrupt(index, 0x0000_0008);
        }
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

fn parse_options(arguments: &[u8]) -> Option<Options> {
    let mut options = Options {
        size_mib: None,
        passes: 1,
        walk: true,
        addr: true,
        moving: true,
        random: true,
        hold_s: 0,
        frames: true,
        seed: 1,
        inject: None,
    };
    // The kernel passes the text after `tape run`, which starts with the
    // tape's own name.
    let words = arguments.split(|&byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'));
    for word in words.filter(|word| !word.is_empty() && *word != b"memtest") {
        let equals = word.iter().position(|&byte| byte == b'=')?;
        let (key, value) = (&word[..equals], &word[equals + 1..]);
        match key {
            b"size" => options.size_mib = Some(parse_u32(value).filter(|&mib| mib >= 1)?),
            b"passes" => options.passes = parse_u32(value)?,
            b"hold" => options.hold_s = parse_u32(value)?,
            b"seed" => options.seed = parse_u32(value)?,
            b"inject" => options.inject = Some(parse_u32(value)? as usize),
            b"frames" => match value {
                b"on" => options.frames = true,
                b"off" => options.frames = false,
                _ => return None,
            },
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
    }
    Some(options)
}

fn print_hex(putc: Putc, value: u32) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for shift in (0..8).rev() {
        putc(u32::from(DIGITS[(value >> (shift * 4) & 0xf) as usize]));
    }
}

/// `1234 ms` style times would need floats; seconds with one decimal do not.
fn print_seconds(putc: Putc, micros: u64) {
    let tenths = (micros / 100_000) as u32;
    print_u32(putc, tenths / 10);
    putc(u32::from(b'.'));
    print_u32(putc, tenths % 10);
    print_bytes(putc, b" s");
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
    _nic_max_frame_len: extern "C" fn() -> u32,
    _ticks: extern "C" fn() -> u64,
    _nic_count: extern "C" fn() -> u32,
    _nic_mac_at: extern "C" fn(u32, u32) -> u32,
    _nic_send_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_receive_at: extern "C" fn(u32, u32, u32) -> u32,
    _nic_set_promiscuous_at: extern "C" fn(u32, u32) -> u32,
    _terminal_position: extern "C" fn() -> u32,
    _fs_file_length: extern "C" fn(u32, u32, u32) -> u32,
    _fs_read: extern "C" fn(u32, u32, u32, u32, u32) -> u32,
    _fs_write: extern "C" fn(u32, u32, u32, u32) -> u32,
    _fs_remove: extern "C" fn(u32, u32) -> u32,
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
              passes=0 repeats until Escape; hold=S adds a retention test.\n",
        );
        return;
    };

    // Claim memory: the frame pool first (it covers the low 128 MiB, which the
    // big pool cannot), then the big pool.
    let mut blocks = [Block { address: 0, bytes: 0, from_frames: false }; MAX_REGIONS];
    let mut count = 0;
    let mut remaining = options.size_mib.map_or(u32::MAX, |mib| mib.saturating_mul(MIB));
    if options.frames {
        claim(&mut blocks, &mut count, &mut remaining, FRAME_POOL_MAX, 4096, MIN_FRAME_BLOCK, true, alloc_frames);
    }
    claim(&mut blocks, &mut count, &mut remaining, BIG_POOL_MAX, MIB, MIB, false, alloc);
    if count == 0 {
        print_bytes(putc, b"memtest: no free memory could be claimed (try frames=on; the big pool\n");
        print_bytes(putc, b"only exists above 128 MiB).\n");
        return;
    }
    let mut total_bytes = 0u64;
    print_bytes(putc, b"fos memtest: claimed");
    for (index, block) in blocks[..count].iter().enumerate() {
        total_bytes += u64::from(block.bytes);
        print_bytes(putc, b"\nblock ");
        print_u32(putc, index as u32 + 1);
        print_bytes(putc, b": 0x");
        print_hex(putc, block.address);
        print_bytes(putc, b" + ");
        print_u32(putc, block.bytes / 1024);
        print_bytes(putc, if block.from_frames { b" KiB (frame pool)" } else { b" KiB (big pool)" });
    }
    print_bytes(putc, b"\ntesting ");
    print_u32(putc, (total_bytes / u64::from(MIB)) as u32);
    print_bytes(putc, b" MiB; Escape stops. Not covered: the tape's own 4 MiB, the kernel's\n");
    print_bytes(putc, b"low memory, the kernel's 4 MiB frame reserve, and RAM above 2 GiB.\n");

    let mut control = Kernel { should_abort, inject: options.inject, aborted: false };
    let mut errors = Errors::new();
    let started = micros();
    let mut printed = 0;
    let mut pass = 0u32;
    'passes: while options.passes == 0 || pass < options.passes {
        pass += 1;
        let pass_started = micros();
        let pass_errors = errors.total;
        for test in [Test::Address, Test::Walking, Test::Moving, Test::Random, Test::Retention] {
            let enabled = match test {
                Test::Address => options.addr,
                Test::Walking => options.walk,
                Test::Moving => options.moving,
                Test::Random => options.random,
                Test::Retention => options.hold_s > 0,
            };
            if !enabled {
                continue;
            }
            let test_started = micros();
            let before = errors.total;
            print_bytes(putc, b"pass ");
            print_u32(putc, pass);
            print_bytes(putc, b" ");
            print_bytes(putc, test.name());
            print_bytes(putc, b" ... ");
            for block in &blocks[..count] {
                let region = unsafe { Region::new(block.address as *mut u32, (block.bytes / 4) as usize, block.address) };
                let finished = match test {
                    Test::Address => address_test(&region, &mut errors, &mut control),
                    Test::Walking => walking_test(&region, &mut errors, &mut control),
                    Test::Moving => moving_test(&region, &mut errors, &mut control),
                    Test::Random => random_test(&region, &mut errors, &mut control, options.seed.wrapping_add(pass)),
                    Test::Retention => retention_test(&region, &mut errors, &mut control, options.hold_s, micros),
                };
                if !finished {
                    break;
                }
            }
            if errors.total == before {
                print_bytes(putc, if control.aborted { b"stopped" } else { b"ok" });
            } else {
                print_u32(putc, errors.total - before);
                print_bytes(putc, b" ERRORS");
            }
            print_bytes(putc, b" (");
            print_seconds(putc, micros() - test_started);
            print_bytes(putc, b")\n");
            while printed < errors.recorded_count {
                let mismatch = errors.recorded[printed];
                print_bytes(putc, b"  0x");
                print_hex(putc, mismatch.address);
                print_bytes(putc, b" want ");
                print_hex(putc, mismatch.expected);
                print_bytes(putc, b" got ");
                print_hex(putc, mismatch.actual);
                print_bytes(putc, b" (");
                print_bytes(putc, mismatch.test.name());
                print_bytes(putc, b")\n");
                printed += 1;
            }
            if control.aborted {
                break 'passes;
            }
        }
        print_bytes(putc, b"pass ");
        print_u32(putc, pass);
        print_bytes(putc, b" done in ");
        print_seconds(putc, micros() - pass_started);
        print_bytes(putc, b", ");
        print_u32(putc, errors.total - pass_errors);
        print_bytes(putc, b" errors\n");
    }

    // The verdict, with the evidence a failing module needs: which bits and
    // which megabytes.
    print_bytes(putc, if control.aborted { b"memtest stopped: " } else { b"memtest finished: " });
    print_u32(putc, pass);
    print_bytes(putc, b" pass(es), ");
    print_u32(putc, (errors.bytes_checked / u64::from(MIB)) as u32);
    print_bytes(putc, b" MiB compared in ");
    print_seconds(putc, micros() - started);
    print_bytes(putc, b"\n");
    if errors.total == 0 {
        print_bytes(putc, b"RESULT: PASS, no errors\n");
    } else {
        print_bytes(putc, b"RESULT: FAIL, ");
        print_u32(putc, errors.total);
        print_bytes(putc, b" errors\nbit flips:");
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

    for block in &blocks[..count] {
        if block.from_frames {
            free_frames(block.address, block.bytes);
        } else {
            free(block.address, block.bytes);
        }
    }
}

/// Retention: fill with a pattern, leave it alone for `seconds`, verify; then
/// the complement. A cell that leaks charge (or a refresh that is too slow)
/// only shows up when the data has sat idle.
fn retention_test(
    region: &Region,
    errors: &mut Errors,
    control: &mut Kernel,
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
