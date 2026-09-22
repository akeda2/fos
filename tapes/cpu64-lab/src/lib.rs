#![no_std]

use core::arch::asm;
use core::arch::x86_64::__cpuid_count;
use core::panic::PanicInfo;

const REPORT_MAGIC: u32 = 0x4350_3634; // "CP64"
const REPORT_VERSION: u32 = 1;

const MODE_PROBE: u32 = 1;
const MODE_X87: u32 = 2;
const MODE_MMX: u32 = 3;
const MODE_SSE: u32 = 4;
const MODE_ALL: u32 = 5;

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

const TEST_X87_RAN: u32 = 1 << 0;
const TEST_X87_PASS: u32 = 1 << 1;
const TEST_MMX_RAN: u32 = 1 << 2;
const TEST_MMX_PASS: u32 = 1 << 3;
const TEST_SSE_RAN: u32 = 1 << 4;
const TEST_SSE_PASS: u32 = 1 << 5;

#[repr(C)]
struct Report {
    magic: u32,
    version: u32,
    mode: u32,
    support: u32,
    tests: u32,
}

#[derive(Copy, Clone)]
enum Mode {
    Probe,
    X87,
    Mmx,
    Sse,
    All,
}

impl Mode {
    fn code(self) -> u32 {
        match self {
            Mode::Probe => MODE_PROBE,
            Mode::X87 => MODE_X87,
            Mode::Mmx => MODE_MMX,
            Mode::Sse => MODE_SSE,
            Mode::All => MODE_ALL,
        }
    }
}

fn parse_mode(args: &[u8]) -> Mode {
    let args = trim_ascii_spaces(args);
    let token = args
        .iter()
        .position(|&byte| byte == b' ')
        .map_or(args, |end| &args[..end]);
    if token.is_empty() || token == b"all" {
        Mode::All
    } else if token == b"probe" {
        Mode::Probe
    } else if token == b"x87" {
        Mode::X87
    } else if token == b"mmx" {
        Mode::Mmx
    } else if token == b"sse" {
        Mode::Sse
    } else {
        Mode::All
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

fn detect_support() -> u32 {
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
    if (bits & SUPPORT_XSAVE) != 0 {
        let xcr0 = xgetbv0();
        if (xcr0 & (1 << 1)) != 0 {
            bits |= SUPPORT_XCR0_XMM;
        }
        if (xcr0 & (1 << 2)) != 0 {
            bits |= SUPPORT_XCR0_YMM;
        }
    }
    bits
}

fn run_x87_test() -> bool {
    let value: u64;
    unsafe {
        asm!(
            "fninit",
            "sub rsp, 8",
            "fld1",
            "fstp qword ptr [rsp]",
            "mov {value}, qword ptr [rsp]",
            "add rsp, 8",
            value = lateout(reg) value,
            options(preserves_flags),
        );
    }
    value == 0x3ff0_0000_0000_0000
}

fn run_mmx_test() -> bool {
    let a: u64 = 0x0003_0002_0001_0000;
    let b: u64 = 0x0001_0001_0001_0001;
    let value: u64;
    unsafe {
        asm!(
            "sub rsp, 16",
            "mov qword ptr [rsp], {a}",
            "mov qword ptr [rsp + 8], {b}",
            "movq mm0, qword ptr [rsp]",
            "movq mm1, qword ptr [rsp + 8]",
            "paddw mm0, mm1",
            "movq qword ptr [rsp], mm0",
            "mov {value}, qword ptr [rsp]",
            "emms",
            "add rsp, 16",
            a = in(reg) a,
            b = in(reg) b,
            value = lateout(reg) value,
            options(preserves_flags),
        );
    }
    value == 0x0004_0003_0002_0001
}

fn run_sse_test() -> bool {
    let bits: u32;
    unsafe {
        asm!(
            "sub rsp, 16",
            "xorps xmm0, xmm0",
            "movss xmm1, dword ptr [rip + {one}]",
            "addss xmm0, xmm1",
            "addss xmm0, xmm1",
            "movss dword ptr [rsp], xmm0",
            "mov {bits:e}, dword ptr [rsp]",
            "add rsp, 16",
            one = sym SSE_ONE_F32_BITS,
            bits = lateout(reg) bits,
            options(preserves_flags),
        );
    }
    bits == 0x4000_0000
}

#[unsafe(no_mangle)]
pub extern "C" fn tape64_body(args_ptr: u64, args_len: u64, report_ptr: u64) {
    if report_ptr == 0 {
        return;
    }
    let mode = {
        let args = if args_ptr == 0 || args_len == 0 {
            &[][..]
        } else {
            let length = core::cmp::min(args_len as usize, 256);
            unsafe { core::slice::from_raw_parts(args_ptr as *const u8, length) }
        };
        parse_mode(args)
    };
    let support = detect_support();
    let mut tests = 0_u32;

    match mode {
        Mode::Probe => {}
        Mode::X87 => {
            if (support & SUPPORT_X87_USABLE) != 0 {
                tests |= TEST_X87_RAN;
                if run_x87_test() {
                    tests |= TEST_X87_PASS;
                }
            }
        }
        Mode::Mmx => {
            if (support & SUPPORT_MMX_USABLE) != 0 {
                tests |= TEST_MMX_RAN;
                if run_mmx_test() {
                    tests |= TEST_MMX_PASS;
                }
            }
        }
        Mode::Sse => {
            if (support & SUPPORT_SSE_USABLE) != 0 {
                tests |= TEST_SSE_RAN;
                if run_sse_test() {
                    tests |= TEST_SSE_PASS;
                }
            }
        }
        Mode::All => {
            if (support & SUPPORT_X87_USABLE) != 0 {
                tests |= TEST_X87_RAN;
                if run_x87_test() {
                    tests |= TEST_X87_PASS;
                }
            }
            if (support & SUPPORT_MMX_USABLE) != 0 {
                tests |= TEST_MMX_RAN;
                if run_mmx_test() {
                    tests |= TEST_MMX_PASS;
                }
            }
            if (support & SUPPORT_SSE_USABLE) != 0 {
                tests |= TEST_SSE_RAN;
                if run_sse_test() {
                    tests |= TEST_SSE_PASS;
                }
            }
        }
    }

    let report = unsafe { &mut *(report_ptr as *mut Report) };
    report.magic = REPORT_MAGIC;
    report.version = REPORT_VERSION;
    report.mode = mode.code();
    report.support = support;
    report.tests = tests;
}

#[unsafe(link_section = ".rodata")]
static SSE_ONE_F32_BITS: u32 = 0x3f80_0000;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}
