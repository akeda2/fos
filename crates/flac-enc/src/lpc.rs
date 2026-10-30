//! Linear prediction for the encoder: window, autocorrelation, Levinson-Durbin, coefficient quantisation and the
//! integer residual. The analysis is floating point (`f64`, plain arithmetic only, no libm, so it works in `no_std`);
//! the residual a decoder reproduces is pure integer arithmetic, so the stream never depends on the float results:
//! whatever the analysis computes, the quantised coefficients written to the file are what the residual uses.

/// Highest predictor order tried (libFLAC's `-8` also stops at 12).
pub const MAX_ORDER: usize = 12;

/// Bits per quantised coefficient. The prediction sum is done in 64 bits here and in every decoder that reads 24-bit
/// FLAC (libFLAC, ffmpeg, the in-tree one), so the usual 32-bit overflow limit does not apply.
pub const PRECISION: u32 = 14;

const MAX_SHIFT: i32 = 15;

/// Windowed autocorrelation of `x` for lags 0..=`MAX_ORDER`. The window is flat in the middle half and has a parabolic
/// (Welch) taper over a quarter of the block at each end, a trig-free stand-in for libFLAC's partial Tukey window.
/// `win` is scratch of at least `x.len()`.
pub fn autocorrelate(x: &[i32], win: &mut [f64], ac: &mut [f64; MAX_ORDER + 1]) {
    let n = x.len();
    let taper = (n / 4).max(1);
    for i in 0..n {
        let edge = i.min(n - 1 - i);
        let w = if edge >= taper {
            1.0
        } else {
            let u = (edge as f64 + 0.5) / taper as f64;
            1.0 - (1.0 - u) * (1.0 - u)
        };
        win[i] = x[i] as f64 * w;
    }
    for (lag, a) in ac.iter_mut().enumerate() {
        let mut sum = 0.0;
        for i in lag..n {
            sum += win[i] * win[i - lag];
        }
        *a = sum;
    }
    // A hair of white noise keeps the recursion stable on a pure tone.
    ac[0] += ac[0] * 1e-9 + 1e-9;
}

/// Levinson-Durbin recursion: `lpc[m][..=m]` are the predictor coefficients of order `m + 1`
/// (`x[i] ~ sum lpc[m][j] * x[i - 1 - j]`). Returns how many orders are usable (the recursion stops when the
/// prediction error reaches zero).
pub fn levinson(ac: &[f64; MAX_ORDER + 1], lpc: &mut [[f64; MAX_ORDER]; MAX_ORDER]) -> usize {
    let mut a = [0.0f64; MAX_ORDER];
    let mut err = ac[0];
    let mut usable = 0;
    for m in 0..MAX_ORDER {
        if err <= 0.0 {
            break;
        }
        let mut acc = ac[m + 1];
        for j in 0..m {
            acc -= a[j] * ac[m - j];
        }
        let k = acc / err;
        if !(k > -1.0 && k < 1.0) {
            break;
        }
        let prev = a;
        for j in 0..m {
            a[j] = prev[j] - k * prev[m - 1 - j];
        }
        a[m] = k;
        err *= 1.0 - k * k;
        lpc[m] = a;
        usable = m + 1;
    }
    usable
}

fn round(v: f64) -> i64 {
    if v >= 0.0 { (v + 0.5) as i64 } else { -((-v + 0.5) as i64) }
}

/// Quantises coefficients to `PRECISION` bits with a common shift, feeding each rounding error into the next
/// coefficient (as libFLAC does). None when they are all zero or not finite.
pub fn quantize(a: &[f64]) -> Option<([i32; MAX_ORDER], i32)> {
    let cmax = a.iter().fold(0.0f64, |m, &v| if v.abs() > m { v.abs() } else { m });
    if !(cmax > 0.0 && cmax < 1e12) {
        return None;
    }
    // Exponent of cmax: cmax is in [2^e, 2^(e+1)).
    let e = ((cmax.to_bits() >> 52) & 0x7ff) as i32 - 1023;
    let shift = (PRECISION as i32 - 2 - e).clamp(0, MAX_SHIFT);
    let scale = (1u32 << shift) as f64;
    let limit = (1i64 << (PRECISION - 1)) - 1;
    let mut q = [0i32; MAX_ORDER];
    let mut carry = 0.0;
    for (slot, &v) in q.iter_mut().zip(a) {
        let want = v * scale + carry;
        let got = round(want).clamp(-limit - 1, limit);
        carry = want - got as f64;
        *slot = got as i32;
    }
    Some((q, shift))
}

/// Residual of the predictor `c` (`c.len()` is the order) with right shift `shift`; the first `c.len()` samples
/// are warm-up and not in `res`. False if a residual is too large to code (nothing usable is then in `res`).
pub fn residual(x: &[i32], c: &[i32], shift: i32, res: &mut [i32]) -> bool {
    let p = c.len();
    for i in p..x.len() {
        let mut sum = 0i64;
        for j in 0..p {
            sum += c[j] as i64 * x[i - 1 - j] as i64;
        }
        let r = x[i] as i64 - (sum >> shift);
        if r.unsigned_abs() >= 1 << 30 {
            return false;
        }
        res[i - p] = r as i32;
    }
    true
}

/// A cheap stand-in for log2(v) in units of 1/256 bit: the exponent plus the mantissa used linearly. Only used to
/// rank predictor orders against each other, where its smooth monotone error does not matter.
pub fn log2_q8(v: u64) -> u64 {
    let v = v.max(1);
    let l = 63 - v.leading_zeros() as u64;
    let frac = if l >= 8 { (v >> (l - 8)) & 255 } else { (v << (8 - l)) & 255 };
    l * 256 + frac
}
