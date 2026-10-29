//! Reverberation analysis of one band of an impulse response: the Schroeder backward-integrated decay curve and what is read from it
//! (EDT, T20, T30, clarity, definition). No allocation: the caller supplies one work buffer as long as the response.
//!
//! Method (ISO 3382-2 in spirit, with Chu's noise compensation and a Lundeby-like truncation):
//! 1. The response starts at its direct sound (`direct_sound`), so t = 0 is the loudest sample.
//! 2. The noise floor is the mean power of the last tenth of the capture (the room has stopped by then; the capture must be
//!    long enough for that, `Report::dynamic_db` tells when it was not).
//! 3. The power envelope, averaged over a short window, is followed until it first comes within `TRUNCATE_DB` of the noise.
//!    The decay is integrated only up to there, with the noise power subtracted, so the noise cannot bend the tail.
//! 4. The integral from t to the end, in dB re its start, is the decay curve; straight lines fitted to it give the times, each
//!    scaled to 60 dB: EDT from 0 to -10 dB, T20 from -5 to -25 dB, T30 from -5 to -35 dB.

use dsp::math::ln64;

/// How far above the noise (dB) the smoothed envelope may end: the decay is cut where it first gets this close.
pub const TRUNCATE_DB: f64 = 6.0;
/// Peak-to-noise (dB) a band needs before EDT, T20 and T30 are reported (fit range + 10 dB; EDT's 10 dB range + 10).
pub const EDT_NEEDS_DB: f32 = 20.0;
pub const T20_NEEDS_DB: f32 = 35.0;
pub const T30_NEEDS_DB: f32 = 45.0;
/// The decay curve below this (dB) and after the cut-off is stored as this.
pub const FLOOR_DB: f32 = -200.0;

/// What one band measured. A time that could not be measured is NaN.
#[derive(Clone, Copy, Debug)]
pub struct Report {
    pub centre_hz: f32,
    /// Early decay time (seconds, scaled to 60 dB, from the first 10 dB).
    pub edt: f32,
    pub t20: f32,
    pub t30: f32,
    /// Clarity (dB): early (first 50 ms / 80 ms) against late energy.
    pub c50: f32,
    pub c80: f32,
    /// Definition D50: the share (0..1) of the energy that arrives in the first 50 ms.
    pub d50: f32,
    /// Peak of the response above the noise floor, dB. T20 wants 35, T30 wants 45 (ISO 3382): the tape flags less.
    pub dynamic_db: f32,
    /// Seconds of response that was used before the decay met the noise.
    pub used_s: f32,
    /// The T20 fit's line: dB at t = 0 and dB per second (NaN without a T20), for drawing over the decay curve.
    pub line_db: f32,
    pub slope: f32,
}

impl Report {
    pub const NONE: Report = Report {
        centre_hz: f32::NAN,
        edt: f32::NAN,
        t20: f32::NAN,
        t30: f32::NAN,
        c50: f32::NAN,
        c80: f32::NAN,
        d50: f32::NAN,
        dynamic_db: f32::NAN,
        used_s: 0.0,
        line_db: f32::NAN,
        slope: f32::NAN,
    };
}

/// Index of the direct sound: the sample of largest magnitude.
pub fn direct_sound(ir: &[f32]) -> usize {
    let mut best = (0, 0.0f32);
    for (i, v) in ir.iter().enumerate() {
        if v.abs() > best.1 {
            best = (i, v.abs());
        }
    }
    best.0
}

fn db(ratio: f64) -> f32 {
    (10.0 * ln64(ratio) / core::f64::consts::LN_10) as f32
}

/// Least-squares line through the part of `curve` (dB per sample) between the first samples at or below `from_db` and `to_db`.
/// Returns (dB at t = 0, dB per second) when that part exists inside `limit` samples and falls.
fn fit(curve: &[f32], rate: u32, from_db: f32, to_db: f32, limit: usize) -> Option<(f32, f32)> {
    let limit = limit.min(curve.len());
    let first = curve[..limit].iter().position(|&v| v <= from_db)?;
    let last = first + curve[first..limit].iter().position(|&v| v <= to_db)?;
    if last < first + 8 {
        return None;
    }
    let (mut n, mut st, mut sy, mut stt, mut sty) = (0.0f64, 0.0, 0.0, 0.0, 0.0);
    for (i, &y) in curve.iter().enumerate().take(last + 1).skip(first) {
        let t = i as f64 / rate as f64;
        let y = y as f64;
        n += 1.0;
        st += t;
        sy += y;
        stt += t * t;
        sty += t * y;
    }
    let slope = (n * sty - st * sy) / (n * stt - st * st);
    if !(slope < -1.0) {
        return None;
    }
    Some((((sy - slope * st) / n) as f32, slope as f32))
}

/// Analyses `x`, a (band-filtered) impulse response that starts at its direct sound, at `rate`. On return `x` holds the decay curve
/// in dB (0 at the start, `FLOOR_DB` after the cut-off). `smooth` is the envelope averaging window in samples (about 10 ms, or
/// a few periods of the band's centre frequency for a low band).
pub fn analyse(x: &mut [f32], rate: u32, smooth: usize, centre_hz: f32) -> Report {
    let n = x.len();
    let mut report = Report { centre_hz, ..Report::NONE };
    let smooth = smooth.max(1);
    if n < 8 * smooth {
        x.fill(FLOOR_DB);
        return report;
    }
    for v in x.iter_mut() {
        *v *= *v;
    }
    let tail = &x[n - n / 10..];
    let noise = tail.iter().map(|&v| v as f64).sum::<f64>() / tail.len() as f64;
    let mut peak = 0.0f64;
    let mut cut = n;
    let limit = noise * pow_db(TRUNCATE_DB);
    for (block, chunk) in x.chunks(smooth).enumerate() {
        let mean = chunk.iter().map(|&v| v as f64).sum::<f64>() / chunk.len() as f64;
        peak = peak.max(mean);
        if block > 0 && mean <= limit && cut == n {
            cut = block * smooth;
        }
    }
    report.dynamic_db = db(peak / noise.max(1e-30));
    report.used_s = cut as f32 / rate as f32;
    let total: f64 = x[..cut].iter().map(|&v| (v as f64 - noise).max(0.0)).sum();
    if total <= 0.0 {
        x.fill(FLOOR_DB);
        return report;
    }
    let k50 = (rate as usize * 50 / 1000).min(cut);
    let k80 = (rate as usize * 80 / 1000).min(cut);
    let (mut late50, mut late80) = (0.0, 0.0);
    let mut running = 0.0f64;
    for i in (0..n).rev() {
        if i >= cut {
            x[i] = FLOOR_DB;
            continue;
        }
        running += (x[i] as f64 - noise).max(0.0);
        x[i] = db((running / total).max(1e-20));
        if i == k50 {
            late50 = running;
        }
        if i == k80 {
            late80 = running;
        }
    }
    if k50 < cut {
        report.c50 = db(((total - late50) / late50.max(1e-30)).max(1e-30));
        report.d50 = ((total - late50) / total) as f32;
    }
    if k80 < cut {
        report.c80 = db(((total - late80) / late80.max(1e-30)).max(1e-30));
    }
    // A time is only reported with the dynamic range ISO 3382 asks for: 10 dB beyond the fit range (EDT also wants a margin).
    let time = |f: Option<(f32, f32)>, need_db: f32| match f {
        Some((_, slope)) if report.dynamic_db >= need_db => -60.0 / slope,
        _ => f32::NAN,
    };
    report.edt = time(fit(x, rate, 0.0, -10.0, cut), EDT_NEEDS_DB);
    let t20 = fit(x, rate, -5.0, -25.0, cut);
    report.t20 = time(t20, T20_NEEDS_DB);
    report.t30 = time(fit(x, rate, -5.0, -35.0, cut), T30_NEEDS_DB);
    if let (Some((line, slope)), false) = (t20, report.t20.is_nan()) {
        report.line_db = line;
        report.slope = slope;
    }
    report
}

fn pow_db(db: f64) -> f64 {
    dsp::math::exp64(db * core::f64::consts::LN_10 / 10.0)
}

/// Peak-to-noise of a whole response in dB, a quick "is there a decay in this capture" figure before analysis.
pub fn peak_to_noise_db(ir: &[f32]) -> f32 {
    let n = ir.len();
    if n < 16 {
        return 0.0;
    }
    let tail = &ir[n - n / 10..];
    let noise = tail.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / tail.len() as f64;
    let peak = ir.iter().fold(0.0f32, |m, v| m.max(v.abs())) as f64;
    db(peak * peak / noise.max(1e-30))
}
