#![no_std]

use core::arch::asm;
use core::arch::x86_64::__cpuid_count;
use core::panic::PanicInfo;

const REPORT_MAGIC: u32 = 0x4350_3634; // "CP64"
const REPORT_VERSION: u32 = 1;

const MODE_SCALAR: u32 = 0x10;
const MODE_SSE2: u32 = 0x11;
const MODE_AUTO: u32 = 0x12;

const SUPPORT_X87: u32 = 1 << 0;
const SUPPORT_MMX: u32 = 1 << 1;
const SUPPORT_SSE: u32 = 1 << 2;
const SUPPORT_SSE2: u32 = 1 << 3;
const SUPPORT_XSAVE: u32 = 1 << 4;
const SUPPORT_AVX: u32 = 1 << 5;
const SUPPORT_XCR0_XMM: u32 = 1 << 6;
const SUPPORT_XCR0_YMM: u32 = 1 << 7;
const SUPPORT_X87_USABLE: u32 = 1 << 8;
const SUPPORT_MMX_USABLE: u32 = 1 << 9;
const SUPPORT_SSE_USABLE: u32 = 1 << 10;
const SUPPORT_SSE2_USABLE: u32 = 1 << 11;
const SUPPORT_ARENA: u32 = 1 << 12;

const TEST_RAN: u32 = 1 << 8;
const TEST_USED_SSE2: u32 = 1 << 9;
const TEST_RENDERED: u32 = 1 << 10;

const VGA_BUFFER: *mut u16 = 0xb8000 as *mut u16;
const WIDTH: usize = 80;
const HEIGHT: usize = 25;

/// Capacity of the shim's `report_summary` buffer (see
/// `tapes/longmode64_shim.S`'s file header comment); must match that
/// buffer's reserved size exactly, since this struct is overlaid directly
/// on the shim's own report memory.
const SUMMARY_CAPACITY: usize = 128;

#[repr(C)]
struct Report {
    magic: u32,
    version: u32,
    mode: u32,
    support: u32,
    tests: u32,
    summary: [u8; SUMMARY_CAPACITY],
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Mode {
    Scalar,
    Sse2,
    Auto,
}

impl Mode {
    fn code(self) -> u32 {
        match self {
            Mode::Scalar => MODE_SCALAR,
            Mode::Sse2 => MODE_SSE2,
            Mode::Auto => MODE_AUTO,
        }
    }
}

fn trim_ascii_spaces(mut bytes: &[u8]) -> &[u8] {
    while let Some((&first, rest)) = bytes.split_first() {
        if first == b' ' {
            bytes = rest;
        } else {
            break;
        }
    }
    while let Some((&last, rest)) = bytes.split_last() {
        if last == b' ' {
            bytes = rest;
        } else {
            break;
        }
    }
    bytes
}

fn take_word(bytes: &[u8]) -> (&[u8], &[u8]) {
    let bytes = trim_ascii_spaces(bytes);
    match bytes.iter().position(|&byte| byte == b' ') {
        Some(index) => (&bytes[..index], &bytes[index + 1..]),
        None => (bytes, &[]),
    }
}

fn parse_u32_ascii(bytes: &[u8]) -> Option<u32> {
    if bytes.is_empty() {
        return None;
    }
    let mut value = 0_u32;
    for &byte in bytes {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value.checked_mul(10)?.checked_add(u32::from(byte - b'0'))?;
    }
    Some(value)
}

fn parse_args(args: &[u8]) -> (Mode, u32) {
    let (mode_word, rest) = take_word(args);
    let mode = if mode_word == b"scalar" {
        Mode::Scalar
    } else if mode_word == b"sse2" {
        Mode::Sse2
    } else {
        Mode::Auto
    };
    let (seconds_word, _) = take_word(rest);
    let seconds = parse_u32_ascii(seconds_word).unwrap_or(10).clamp(1, 120);
    (mode, seconds)
}

fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    let result = __cpuid_count(leaf, subleaf);
    (result.eax, result.ebx, result.ecx, result.edx)
}

fn xgetbv0() -> u64 {
    let eax: u32;
    let edx: u32;
    unsafe {
        asm!(
            "xgetbv",
            in("ecx") 0_u32,
            lateout("eax") eax,
            lateout("edx") edx,
            options(nomem, nostack),
        );
    }
    u64::from(eax) | (u64::from(edx) << 32)
}

fn read_cr0() -> u64 {
    let value: u64;
    unsafe {
        asm!("mov {}, cr0", lateout(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

fn read_cr4() -> u64 {
    let value: u64;
    unsafe {
        asm!("mov {}, cr4", lateout(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Reads the timestamp counter. Used only to enforce a real minimum
/// wall-clock runtime for the demo loop below: the shim cannot safely call
/// back into 32-bit `ticks`/`should_abort` callbacks once inside this
/// 64-bit excursion (see `tapes/longmode64_shim.S`'s file header comment),
/// so `rdtsc`, calibrated against a `cycles_per_second` figure the shim
/// measured just before the mode switch, is this body's only clock.
fn rdtsc() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        asm!("rdtsc", lateout("eax") lo, lateout("edx") hi, options(nomem, nostack, preserves_flags));
    }
    (u64::from(hi) << 32) | u64::from(lo)
}

/// A tiny fixed-capacity ASCII text builder, used to assemble the
/// human-readable capability/usage summary line written into the shim's
/// `report_summary` buffer. No heap exists on this target, so this is a
/// plain `[u8; N]` buffer plus a length, exactly the same shape the
/// kernel's own line editor and this project's other no-alloc text
/// helpers already use.
struct TextBuf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> TextBuf<N> {
    fn new() -> Self {
        TextBuf { bytes: [0; N], len: 0 }
    }

    fn push_bytes(&mut self, extra: &[u8]) {
        for &byte in extra {
            // Leave the final byte permanently zero, so this buffer is
            // always NUL-terminated for the shim's `print_string` even if
            // truncated here.
            if self.len + 1 >= N {
                return;
            }
            self.bytes[self.len] = byte;
            self.len += 1;
        }
    }

    fn push_decimal(&mut self, mut value: u32) {
        let mut scratch = [0u8; 10];
        let mut length = 0usize;
        if value == 0 {
            scratch[0] = b'0';
            length = 1;
        } else {
            while value != 0 {
                scratch[length] = b'0' + (value % 10) as u8;
                value /= 10;
                length += 1;
            }
        }
        for index in (0..length).rev() {
            self.push_bytes(&[scratch[index]]);
        }
    }
}

fn detect_support(arena_len: usize) -> u32 {
    let (_, _, ecx, edx) = cpuid(1, 0);
    let cr0 = read_cr0();
    let cr4 = read_cr4();
    let mut bits = 0_u32;
    if (edx & (1 << 0)) != 0 {
        bits |= SUPPORT_X87;
    }
    if (edx & (1 << 23)) != 0 {
        bits |= SUPPORT_MMX;
    }
    if (edx & (1 << 25)) != 0 {
        bits |= SUPPORT_SSE;
    }
    if (edx & (1 << 26)) != 0 {
        bits |= SUPPORT_SSE2;
    }
    if (ecx & (1 << 26)) != 0 {
        bits |= SUPPORT_XSAVE;
    }
    if (ecx & (1 << 28)) != 0 {
        bits |= SUPPORT_AVX;
    }
    let x87_usable = (bits & SUPPORT_X87) != 0 && (cr0 & ((1 << 2) | (1 << 3))) == 0;
    if x87_usable {
        bits |= SUPPORT_X87_USABLE;
    }
    if (bits & SUPPORT_MMX) != 0 && x87_usable {
        bits |= SUPPORT_MMX_USABLE;
    }
    if (bits & SUPPORT_SSE) != 0 && x87_usable && (cr4 & (1 << 9)) != 0 {
        bits |= SUPPORT_SSE_USABLE;
    }
    if (bits & SUPPORT_SSE2) != 0 && (bits & SUPPORT_SSE_USABLE) != 0 {
        bits |= SUPPORT_SSE2_USABLE;
    }
    if (bits & SUPPORT_XSAVE) != 0 && (cr4 & (1 << 18)) != 0 {
        let xcr0 = xgetbv0();
        if (xcr0 & (1 << 1)) != 0 {
            bits |= SUPPORT_XCR0_XMM;
        }
        if (xcr0 & (1 << 2)) != 0 {
            bits |= SUPPORT_XCR0_YMM;
        }
    }
    if arena_len >= (1 << 20) {
        bits |= SUPPORT_ARENA;
    }
    bits
}

fn update_scalar(words: &mut [u32], frame: u32) {
    let mix = frame.wrapping_mul(0x9e37_79b9).wrapping_add(0x7f4a_7c15);
    for value in words {
        let mut x = *value;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        *value = x.wrapping_add(mix);
    }
}

unsafe fn update_sse2(words: &mut [u32], frame: u32) {
    let lanes = words.len() & !3;
    let add = frame.wrapping_mul(31).wrapping_add(17);
    let mut index = 0;
    while index < lanes {
        let pointer = unsafe { words.as_mut_ptr().add(index) };
        unsafe {
            asm!(
                "movdqu xmm0, [{pointer}]",
                "movdqa xmm1, xmm0",
                "pslld xmm1, 13",
                "pxor xmm0, xmm1",
                "movdqa xmm1, xmm0",
                "psrld xmm1, 17",
                "pxor xmm0, xmm1",
                "movd xmm1, {add:e}",
                "pshufd xmm1, xmm1, 0",
                "paddd xmm0, xmm1",
                "movdqu [{pointer}], xmm0",
                pointer = in(reg) pointer,
                add = in(reg) add,
                options(nostack, preserves_flags),
            );
        }
        index += 4;
    }
    if lanes != words.len() {
        update_scalar(&mut words[lanes..], frame);
    }
}

fn write_cell(index: usize, character: u8, attribute: u8) {
    unsafe {
        VGA_BUFFER
            .add(index)
            .write_volatile((u16::from(attribute) << 8) | u16::from(character));
    }
}

fn write_text(row: usize, column: usize, bytes: &[u8], attribute: u8) {
    let mut col = column;
    for &byte in bytes {
        if col >= WIDTH {
            break;
        }
        write_cell(row * WIDTH + col, byte, attribute);
        col += 1;
    }
}

fn write_u32_decimal(row: usize, mut column: usize, mut value: u32, attribute: u8) -> usize {
    let mut scratch = [0u8; 10];
    let mut length = 0usize;
    if value == 0 {
        scratch[0] = b'0';
        length = 1;
    } else {
        while value != 0 {
            scratch[length] = b'0' + (value % 10) as u8;
            value /= 10;
            length += 1;
        }
    }
    for index in (0..length).rev() {
        write_cell(row * WIDTH + column, scratch[index], attribute);
        column += 1;
    }
    column
}

/// Bhaskara I's ~7th-century integer approximation of sin(degrees), scaled
/// by 1000 (so the true range [-1, 1] becomes roughly [-1000, 1000]). This
/// keeps the plasma field's math entirely in integer arithmetic, which is
/// both simpler and faster than pulling in a soft-float sine on this
/// SSE-disabled, softfloat-ABI target (see targets/x86_64-fos-tape.json).
fn sin1000(degrees: i32) -> i32 {
    let mut normalized = degrees % 360;
    if normalized < 0 {
        normalized += 360;
    }
    let (sign, x) = if normalized < 180 {
        (1_i64, normalized as i64)
    } else {
        (-1_i64, (normalized - 180) as i64)
    };
    let numerator = 4 * x * (180 - x) * 1000;
    let denominator = 40500 - x * (180 - x);
    if denominator == 0 {
        return 0;
    }
    (sign * (numerator / denominator)) as i32
}

/// Plain integer square root (Newton's method), used to turn a screen cell's
/// (dx, dy) offset from center into a radius for the ripple term below.
fn isqrt(value: u32) -> u32 {
    if value == 0 {
        return 0;
    }
    let mut x = value;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + value / x) / 2;
    }
    x
}

/// Renders one frame of a classic four-wave plasma field (horizontal ripple,
/// vertical ripple, diagonal ripple, and a radial ripple from screen center),
/// each an integer sine of position and time. `sample_words` is the slice of
/// the arena the vector update just touched this frame; a handful of bright
/// "particle" sparkles are placed using its live values so the on-screen
/// picture visibly reflects the ongoing scalar/SSE2 vector workload rather
/// than only being background computation.
fn render_frame(sample_words: &[u32], frame: u32, mode: Mode, arena_len: usize, use_sse2: bool) {
    const PALETTE: &[u8; 10] = b" .:-=+*#%@";
    let phase = (frame as i32).wrapping_mul(6);
    let center_x = (WIDTH / 2) as i32;
    let center_y = (HEIGHT / 2) as i32;
    for row in 0..(HEIGHT - 1) {
        for column in 0..WIDTH {
            let cell = row * WIDTH + column;
            let dx = column as i32 - center_x;
            // VGA text cells are about twice as tall as wide, so scale the
            // vertical offset to keep the radial ripple visually circular.
            let dy = (row as i32 - center_y) * 2;
            let radius = isqrt((dx * dx + dy * dy) as u32) as i32;
            let horizontal = sin1000(column as i32 * 9 + phase * 2);
            let vertical = sin1000(row as i32 * 16 - phase);
            let radial = sin1000(radius * 7 - phase * 3);
            let diagonal = sin1000((column as i32 + row as i32) * 5 + phase / 2);
            let sum = horizontal + vertical + radial + diagonal; // roughly -4000..4000
            let level = ((sum + 4000).clamp(0, 7999) as u32 * (PALETTE.len() as u32 - 1)) / 8000;
            let shade = PALETTE[level as usize];
            let color = (((sum + 4000) / 500) as u8 & 0x07) + 8;
            write_cell(cell, shade, color);
        }
    }

    let particle_count = core::cmp::min(16, sample_words.len());
    for sample in sample_words.iter().take(particle_count) {
        let px = (*sample as usize) % WIDTH;
        let py = ((*sample >> 16) as usize) % (HEIGHT - 1);
        write_cell(py * WIDTH + px, b'*', 0x0f);
    }

    for column in 0..WIDTH {
        write_cell((HEIGHT - 1) * WIDTH + column, b' ', 0x1f);
    }
    let label = match mode {
        Mode::Scalar => b"cpu64-show scalar".as_slice(),
        Mode::Sse2 => b"cpu64-show sse2".as_slice(),
        Mode::Auto if use_sse2 => b"cpu64-show auto->sse2".as_slice(),
        Mode::Auto => b"cpu64-show auto->scalar".as_slice(),
    };
    write_text(HEIGHT - 1, 0, label, 0x1f);
    write_text(HEIGHT - 1, 24, b" frame ", 0x1f);
    let mut next = write_u32_decimal(HEIGHT - 1, 31, frame + 1, 0x1f);
    write_text(HEIGHT - 1, next, b" arenaKiB ", 0x1f);
    next += 10;
    let _ = write_u32_decimal(HEIGHT - 1, next, (arena_len / 1024) as u32, 0x1f);
}

fn seed_words(words: &mut [u32]) {
    let mut x = 0x1234_5678_u32;
    for (index, word) in words.iter_mut().enumerate() {
        x = x.wrapping_mul(1664525).wrapping_add(1013904223 ^ index as u32);
        *word = x.rotate_left((index as u32) & 31);
    }
}

/// A generous backstop against a broken/implausible calibration (see
/// `tape64_body`'s sanity check on `cycles_per_second`) -- normal runs stop
/// long before this via the elapsed-cycles check instead.
const MAX_FRAMES: u32 = 2_000_000;

fn run_demo(words: &mut [u32], requested_mode: Mode, target_cycles: u64, support: u32) -> (u32, u32) {
    let can_use_sse2 = (support & SUPPORT_SSE2_USABLE) != 0;
    let use_sse2 = matches!(requested_mode, Mode::Sse2) || (requested_mode == Mode::Auto && can_use_sse2);
    let work_words = core::cmp::min(words.len(), 262_144);
    seed_words(words);
    let mut tests = TEST_RAN;
    if use_sse2 && can_use_sse2 {
        tests |= TEST_USED_SSE2;
    }
    let start_tsc = rdtsc();
    let mut frame = 0_u32;
    loop {
        let start = ((frame as usize) * work_words) % words.len();
        let end = core::cmp::min(start + work_words, words.len());
        if use_sse2 && can_use_sse2 {
            unsafe { update_sse2(&mut words[start..end], frame) };
            if end - start < work_words {
                let rest = work_words - (end - start);
                unsafe { update_sse2(&mut words[..rest], frame.wrapping_add(1)) };
            }
        } else {
            update_scalar(&mut words[start..end], frame);
            if end - start < work_words {
                let rest = work_words - (end - start);
                update_scalar(&mut words[..rest], frame.wrapping_add(1));
            }
        }
        render_frame(&words[start..end], frame, requested_mode, words.len() * 4, use_sse2 && can_use_sse2);
        frame += 1;
        if frame >= MAX_FRAMES {
            break;
        }
        if rdtsc().wrapping_sub(start_tsc) >= target_cycles {
            break;
        }
    }
    (tests | TEST_RENDERED, frame)
}

/// Same rendering/update logic as `run_demo`, but bounded purely by frame
/// count rather than elapsed `rdtsc` cycles -- used only when
/// `cycles_per_second` calibration looks untrustworthy (see
/// `tape64_body`'s `MIN_TRUSTED_CYCLES_PER_SECOND` check), so the demo
/// still terminates promptly instead of trusting a bogus time target.
fn run_demo_fixed_frames(words: &mut [u32], requested_mode: Mode, frames: u32, support: u32) -> (u32, u32) {
    let can_use_sse2 = (support & SUPPORT_SSE2_USABLE) != 0;
    let use_sse2 = matches!(requested_mode, Mode::Sse2) || (requested_mode == Mode::Auto && can_use_sse2);
    let work_words = core::cmp::min(words.len(), 262_144);
    seed_words(words);
    let mut tests = TEST_RAN;
    if use_sse2 && can_use_sse2 {
        tests |= TEST_USED_SSE2;
    }
    for frame in 0..frames {
        let start = ((frame as usize) * work_words) % words.len();
        let end = core::cmp::min(start + work_words, words.len());
        if use_sse2 && can_use_sse2 {
            unsafe { update_sse2(&mut words[start..end], frame) };
            if end - start < work_words {
                let rest = work_words - (end - start);
                unsafe { update_sse2(&mut words[..rest], frame.wrapping_add(1)) };
            }
        } else {
            update_scalar(&mut words[start..end], frame);
            if end - start < work_words {
                let rest = work_words - (end - start);
                update_scalar(&mut words[..rest], frame.wrapping_add(1));
            }
        }
        render_frame(&words[start..end], frame, requested_mode, words.len() * 4, use_sse2 && can_use_sse2);
    }
    (tests | TEST_RENDERED, frames)
}

/// Below this, a `cycles_per_second` figure is treated as an unreliable
/// calibration (no real x86-64 CPU runs this slow) rather than trusted --
/// see the file header comment in `tapes/longmode64_shim.S`.
const MIN_TRUSTED_CYCLES_PER_SECOND: u64 = 100_000_000;

/// A safe fallback frame count used only when `cycles_per_second`
/// calibration looks untrustworthy (see above): a fixed-length run instead
/// of a real time-based one, so the demo still terminates promptly.
const FALLBACK_FRAMES: u32 = 240;

#[unsafe(no_mangle)]
pub extern "C" fn tape64_body(
    args_ptr: u64,
    args_len: u64,
    report_ptr: u64,
    arena_ptr: u64,
    arena_len: u64,
    cycles_per_second: u64,
) {
    if report_ptr == 0 {
        return;
    }
    let args = if args_ptr == 0 || args_len == 0 {
        &[][..]
    } else {
        let length = core::cmp::min(args_len as usize, 256);
        unsafe { core::slice::from_raw_parts(args_ptr as *const u8, length) }
    };
    let (requested_mode, seconds) = parse_args(args);
    let support = detect_support(arena_len as usize);
    let calibration_trusted = cycles_per_second >= MIN_TRUSTED_CYCLES_PER_SECOND;
    let target_cycles = cycles_per_second.saturating_mul(u64::from(seconds));
    let (tests, frame_count) = if arena_ptr != 0 && arena_len >= 4096 {
        let words = unsafe { core::slice::from_raw_parts_mut(arena_ptr as *mut u32, (arena_len as usize) / 4) };
        if calibration_trusted {
            run_demo(words, requested_mode, target_cycles, support)
        } else {
            run_demo_fixed_frames(words, requested_mode, FALLBACK_FRAMES, support)
        }
    } else {
        let mut fallback = [0u32; 1024];
        if calibration_trusted {
            run_demo(&mut fallback, requested_mode, target_cycles, support)
        } else {
            run_demo_fixed_frames(&mut fallback, requested_mode, FALLBACK_FRAMES, support)
        }
    };

    let report = unsafe { &mut *(report_ptr as *mut Report) };
    report.magic = REPORT_MAGIC;
    report.version = REPORT_VERSION;
    report.mode = requested_mode.code();
    report.support = support;
    report.tests = tests;

    let mut summary: TextBuf<SUMMARY_CAPACITY> = TextBuf::new();
    summary.push_bytes(b"cpu64-show: cpu supports");
    if (support & SUPPORT_X87) != 0 {
        summary.push_bytes(b" x87");
    }
    if (support & SUPPORT_MMX) != 0 {
        summary.push_bytes(b" mmx");
    }
    if (support & SUPPORT_SSE) != 0 {
        summary.push_bytes(b" sse");
    }
    if (support & SUPPORT_SSE2) != 0 {
        summary.push_bytes(b" sse2");
    }
    summary.push_bytes(b"; used");
    if (tests & TEST_USED_SSE2) != 0 {
        summary.push_bytes(b" sse2");
    } else {
        summary.push_bytes(b" scalar");
    }
    summary.push_bytes(b" across ");
    summary.push_decimal(frame_count);
    summary.push_bytes(b" frames");
    if calibration_trusted {
        summary.push_bytes(b" in ~");
        summary.push_decimal(seconds);
        summary.push_bytes(b"s");
    } else {
        summary.push_bytes(b" (uncalibrated timing; fixed frame count used)");
    }
    summary.push_bytes(b"\n");
    report.summary[..summary.len].copy_from_slice(&summary.bytes[..summary.len]);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
