//! Elementary functions without libm (`core` has none). Everything is computed in f64 with short polynomial series and
//! rounded to f32: accurate to about 1e-9 relative, far below what a 16-bit audio spectrum can show.

use core::f64::consts::{LN_10, LN_2, PI};

pub fn sqrt64(x: f64) -> f64 {
    use core::arch::x86_64::{_mm_cvtsd_f64, _mm_set_sd, _mm_sqrt_sd};
    unsafe { _mm_cvtsd_f64(_mm_sqrt_sd(_mm_set_sd(0.0), _mm_set_sd(x))) }
}

pub fn sqrt(x: f32) -> f32 {
    sqrt64(x as f64) as f32
}

pub fn floor64(x: f64) -> f64 {
    if x.abs() >= 4.5e15 {
        return x;
    }
    let truncated = x as i64 as f64;
    if truncated > x { truncated - 1.0 } else { truncated }
}

/// Sine and cosine of `x` radians, as a pair `(sin, cos)`.
pub fn sin_cos64(x: f64) -> (f64, f64) {
    let quadrants = floor64(x * (2.0 / PI) + 0.5);
    let r = x - quadrants * (PI / 2.0);
    let r2 = r * r;
    // Taylor series on |r| <= pi/4 (next term below 1e-12).
    let sin = r * (1.0 + r2 * (-1.0 / 6.0 + r2 * (1.0 / 120.0 + r2 * (-1.0 / 5040.0 + r2 * (1.0 / 362880.0 + r2 * (-1.0 / 39916800.0 + r2 * (1.0 / 6227020800.0)))))));
    let cos = 1.0 + r2 * (-0.5 + r2 * (1.0 / 24.0 + r2 * (-1.0 / 720.0 + r2 * (1.0 / 40320.0 + r2 * (-1.0 / 3628800.0 + r2 * (1.0 / 479001600.0))))));
    match (quadrants as i64).rem_euclid(4) {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

pub fn sin(x: f32) -> f32 {
    sin_cos64(x as f64).0 as f32
}

pub fn cos(x: f32) -> f32 {
    sin_cos64(x as f64).1 as f32
}

/// Natural logarithm; `x <= 0` gives -infinity-ish (-1e30), which callers clamp for display.
pub fn ln64(x: f64) -> f64 {
    if x <= 0.0 {
        return -1e30;
    }
    let bits = x.to_bits();
    let mut exponent = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | 0x3ff0_0000_0000_0000);
    if m > core::f64::consts::SQRT_2 {
        m *= 0.5;
        exponent += 1;
    }
    // ln m = 2 atanh(s), s = (m-1)/(m+1), |s| < 0.172.
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let series = 2.0 * s * (1.0 + s2 * (1.0 / 3.0 + s2 * (1.0 / 5.0 + s2 * (1.0 / 7.0 + s2 * (1.0 / 9.0 + s2 * (1.0 / 11.0 + s2 * (1.0 / 13.0)))))));
    exponent as f64 * LN_2 + series
}

pub fn log10(x: f32) -> f32 {
    (ln64(x as f64) / LN_10) as f32
}

pub fn exp64(x: f64) -> f64 {
    if x < -700.0 {
        return 0.0;
    }
    let k = floor64(x / LN_2 + 0.5);
    let r = x - k * LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    for n in 1..=13 {
        term *= r / n as f64;
        sum += term;
    }
    sum * f64::from_bits(((k as i64 + 1023) as u64) << 52)
}

/// 10^x.
pub fn pow10(x: f32) -> f32 {
    exp64(x as f64 * LN_10) as f32
}

/// 20*log10 of an amplitude / 10*log10 of a power, floored at `floor_db` so silence stays on the chart.
pub fn power_to_db(power: f32, floor_db: f32) -> f32 {
    let db = 10.0 * log10(power);
    if db < floor_db { floor_db } else { db }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_std() {
        let mut x = -50.0f64;
        while x < 50.0 {
            let (s, c) = sin_cos64(x);
            assert!((s - x.sin()).abs() < 1e-9 && (c - x.cos()).abs() < 1e-9, "sin/cos {x}");
            x += 0.173;
        }
        for &x in &[1e-12f64, 0.003, 0.5, 1.0, 1.4142, 2.0, 10.0, 12345.6, 3e9] {
            assert!((ln64(x) - x.ln()).abs() < 1e-9 * x.ln().abs().max(1.0), "ln {x}");
            assert!((sqrt64(x) - x.sqrt()).abs() < 1e-12 * x.sqrt());
        }
        for &x in &[-80.0f64, -3.5, -0.1, 0.0, 0.7, 5.0, 20.0, 300.0] {
            assert!((exp64(x) - x.exp()).abs() < 1e-9 * x.exp(), "exp {x}");
        }
        assert!((log10(1000.0) - 3.0).abs() < 1e-6);
        assert!((pow10(-2.0) - 0.01).abs() < 1e-8);
        assert_eq!(floor64(-1.5), -2.0);
        assert_eq!(floor64(2.0), 2.0);
    }

    #[test]
    fn db_is_floored() {
        assert_eq!(power_to_db(0.0, -120.0), -120.0);
        assert!((power_to_db(100.0, -120.0) - 20.0).abs() < 1e-5);
    }
}
