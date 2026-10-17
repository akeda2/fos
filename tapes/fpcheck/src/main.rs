//! Checks that a hard-float tape runs: the kernel switched SSE (and AVX2 where the CPU has it) on before the tape started.
//! `tape run fpcheck` prints the level `fp_level` reports and the result of f64 arithmetic, an SSE vector operation, the
//! default MXCSR, stack alignment for 32-byte loads and, with AVX2, an FMA dot product. It prints `fpcheck: ok` when all hold.
//! The tape for `make test-fp` (`tools/test_fp.py`); also a template for the next hard-float tape.
#![no_std]
#![no_main]

use core::arch::x86_64::*;
use tape_rt::{FP_AVX2, FP_NONE, FP_SSE2, TapeApi, println};

#[repr(align(32))]
struct Aligned([f32; 8]);

/// The sum of 1/k^2 converges to pi^2/6 = 1.6449340668...; 1000 terms are within 1e-3.
fn basel() -> f64 {
    let mut sum = 0.0f64;
    let mut k = 1.0f64;
    while k <= 1000.0 {
        sum += 1.0 / (k * k);
        k += 1.0;
    }
    sum
}

#[target_feature(enable = "avx2,fma")]
fn dot_avx2(a: &Aligned, b: &Aligned) -> f32 {
    unsafe {
        let product = _mm256_fmadd_ps(_mm256_load_ps(a.0.as_ptr()), _mm256_load_ps(b.0.as_ptr()), _mm256_setzero_ps());
        let mut lanes = Aligned([0.0; 8]);
        _mm256_store_ps(lanes.0.as_mut_ptr(), product);
        lanes.0.iter().sum()
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let level = tape_rt::fp_level();
    let name = match level {
        FP_NONE => "none",
        FP_SSE2 => "sse2",
        FP_AVX2 => "avx2",
        _ => "?",
    };
    println!("fpcheck: fp level {level} ({name})");
    if level == FP_NONE {
        println!("fpcheck: FAIL no floating point");
        return;
    }
    let mut ok = true;
    let micro = (basel() * 1_000_000.0) as i64;
    println!("fpcheck: sum 1/k^2 = {micro} ppm of 1 (1000 terms: pi^2/6 less 1000 ppm)");
    ok &= (1_643_000..1_645_000).contains(&micro);

    let a = Aligned([1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    let b = Aligned([8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0]);
    let sse = unsafe {
        let product = _mm_mul_ps(_mm_load_ps(a.0.as_ptr()), _mm_load_ps(b.0.as_ptr()));
        let mut lanes = [0.0f32; 4];
        _mm_storeu_ps(lanes.as_mut_ptr(), product);
        lanes.iter().sum::<f32>()
    };
    println!("fpcheck: sse dot of the first four = {sse}");
    ok &= sse == 8.0 + 14.0 + 18.0 + 20.0;

    #[allow(deprecated)]
    let mxcsr = unsafe { _mm_getcsr() };
    println!("fpcheck: mxcsr {mxcsr:#x}");
    ok &= mxcsr & !0x3f == 0x1f80; // all exceptions masked, round to nearest; the low six bits are sticky flags

    if level == FP_AVX2 {
        let dot = unsafe { dot_avx2(&a, &b) }; // fp_level says the CPU and the kernel have AVX2 and FMA on
        println!("fpcheck: avx2 fma dot of eight = {dot}");
        ok &= dot == 120.0;
    }
    println!("{}", if ok { "fpcheck: ok" } else { "fpcheck: FAIL" });
}
