//! Measurement-microphone calibration: from band levels the analyser measured to a per-band correction, and the two text
//! files that carry it (the fos file and a REW-style one). No hardware, no allocation; the `miccal` tape does the audio and
//! the drawing.
//!
//! Everything is in 1/3-octave band levels of the analyser (`crates/ltas`, dB re a full-scale sine); a missing value is NaN.
//! A **correction** is what to add to a band level to get dB SPL: `spl = level + correction`.
//!
//! Three ways to get one (`Method`):
//! * **Coupler**: a closed noise source of known pink-noise level covers the mic. Pink noise has equal power in every
//!   1/3-octave band, so the target is the same in each band and `correction = target - measured`. Absolute.
//! * **Reference**: the same acoustic source measured with a reference mic and then with the mic under test, in the same
//!   place. `correction = reference - measured`, plus the reference mic's own correction when it has one (`ref=` file).
//!   Absolute only with that file or a calibrator tone; else relative (0 dB at 1 kHz).
//! * **Calibrator**: one tone of known SPL (94 dB at 1 kHz is usual) sets a flat sensitivity: `correction = spl - measured`.
//!
//! A tone captured as well shifts a Coupler or Reference result so that the 1 kHz band reads the calibrator's level.
#![no_std]

use core::fmt::{self, Write};
use dsp::math::log10;

/// The frequency at which a relative curve is 0 dB and a calibrator tone is measured.
pub const REF_HZ: f32 = 1000.0;
/// The most points a file can hold (31 bands at 1/3 octave, with room for finer ones).
pub const MAX_POINTS: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Coupler,
    Reference,
    Calibrator,
}

impl Method {
    pub const ALL: [Method; 3] = [Method::Coupler, Method::Reference, Method::Calibrator];

    pub fn name(self) -> &'static str {
        match self {
            Method::Coupler => "coupler",
            Method::Reference => "reference",
            Method::Calibrator => "calibrator",
        }
    }

    pub fn from_name(name: &str) -> Option<Method> {
        Self::ALL.into_iter().find(|m| m.name() == name)
    }

    pub fn next(self) -> Method {
        Self::ALL[(Self::ALL.iter().position(|&m| m == self).unwrap_or(0) + 1) % Self::ALL.len()]
    }
}

/// The level of each band of pink noise whose total over `bands` equal bands is `overall_db`.
pub fn pink_band_level(overall_db: f32, bands: usize) -> f32 {
    overall_db - 10.0 * log10(bands.max(1) as f32)
}

pub struct Inputs<'a> {
    /// Band centres in Hz, the same length as every curve.
    pub centres: &'a [f32],
    /// The mic under test.
    pub measured: &'a [f32],
    /// The reference mic (Reference).
    pub reference: Option<&'a [f32]>,
    /// The reference mic's own correction on the same bands (Reference).
    pub reference_correction: Option<&'a [f32]>,
    /// A calibrator tone's band levels (only the 1 kHz band is used).
    pub tone: Option<&'a [f32]>,
    /// The calibrator tone's SPL.
    pub tone_spl: f32,
    /// The noise source's level in every 1/3-octave band (Coupler).
    pub target_band_db: f32,
}

/// What a correction means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Outcome {
    /// dB SPL = level + correction; false: relative to 1 kHz.
    pub absolute: bool,
}

/// The band nearest `REF_HZ` (in log frequency).
pub fn reference_band(centres: &[f32]) -> Option<usize> {
    (0..centres.len()).min_by(|&a, &b| (log10(centres[a]) - log10(REF_HZ)).abs().total_cmp(&(log10(centres[b]) - log10(REF_HZ)).abs()))
}

fn have(value: f32) -> bool {
    !value.is_nan()
}

/// The correction for `method` into `out`, or what is missing.
pub fn correction(method: Method, inputs: &Inputs, out: &mut [f32]) -> Result<Outcome, &'static str> {
    let count = inputs.centres.len();
    if out.len() < count || inputs.measured.len() < count {
        return Err("curve length");
    }
    let band = reference_band(inputs.centres).ok_or("no bands")?;
    let tone = match inputs.tone {
        Some(tone) if have(tone[band]) => Some(tone[band]),
        Some(_) => return Err("the tone is not measured at 1 kHz"),
        None => None,
    };
    if method == Method::Calibrator {
        let level = tone.ok_or("capture the calibrator tone first")?;
        out[..count].fill(inputs.tone_spl - level);
        return Ok(Outcome { absolute: true });
    }
    let mut absolute = method == Method::Coupler;
    for i in 0..count {
        let level = inputs.measured[i];
        out[i] = match method {
            Method::Coupler => inputs.target_band_db - level,
            _ => {
                let reference = inputs.reference.ok_or("capture the reference mic first")?;
                let own = inputs.reference_correction.map_or(0.0, |c| c[i]);
                reference[i] - level + own
            }
        };
    }
    if method == Method::Reference && inputs.reference_correction.is_some() {
        absolute = true;
    }
    if !(0..count).any(|i| have(out[i])) {
        return Err("capture the mic first");
    }
    if let Some(level) = tone {
        if !have(out[band]) {
            return Err("the 1 kHz band is not measured");
        }
        let shift = inputs.tone_spl - (level + out[band]);
        out[..count].iter_mut().for_each(|v| *v += shift);
        absolute = true;
    } else if !absolute {
        if !have(out[band]) {
            return Err("the 1 kHz band is not measured");
        }
        let at_reference = out[band];
        out[..count].iter_mut().for_each(|v| *v -= at_reference);
    }
    Ok(Outcome { absolute })
}

/// What the files say about a measurement.
pub struct Header<'a> {
    pub name: &'a str,
    pub stamp: &'a str,
    pub method: Method,
    pub rate: u32,
    pub outcome: Outcome,
}

/// The fos calibration file: `#` comment lines, then `centre_hz correction_db` for every measured band. `parse` reads it.
pub fn write_fos(out: &mut impl Write, header: &Header, centres: &[f32], correction: &[f32]) -> fmt::Result {
    writeln!(out, "# miccal: measurement microphone calibration (fos tape miccal) {}", header.stamp)?;
    writeln!(out, "# name {}", header.name)?;
    writeln!(out, "# method {}", header.method.name())?;
    writeln!(out, "# absolute {}", if header.outcome.absolute { "yes" } else { "no" })?;
    writeln!(out, "# rate {} Hz, 1/3-octave bands", header.rate)?;
    writeln!(out, "# spl = analyser level + correction (dB); relative files are 0 dB at 1 kHz")?;
    writeln!(out, "# centre_hz correction_db")?;
    for (hz, db) in centres.iter().zip(correction) {
        if have(*db) {
            writeln!(out, "{hz:.1} {db:.2}")?;
        }
    }
    Ok(())
}

/// A REW-style microphone file: `*` comments, then `frequency deviation` where the deviation is what the mic reads above the
/// true level (the inverse of the correction), relative to 1 kHz. Importing it as an absolute sensitivity is not done:
/// the absolute offset is in a comment.
pub fn write_rew(out: &mut impl Write, header: &Header, centres: &[f32], correction: &[f32]) -> fmt::Result {
    let at = reference_band(centres).map_or(0.0, |b| correction[b]);
    writeln!(out, "* miccal {} {}", header.name, header.stamp)?;
    writeln!(out, "* method {}, rate {} Hz, 1/3-octave bands", header.method.name(), header.rate)?;
    if header.outcome.absolute {
        writeln!(out, "* absolute correction at 1 kHz: {at:+.2} dB (spl = analyser level + correction)")?;
    } else {
        writeln!(out, "* relative to 1 kHz only (no absolute level)")?;
    }
    writeln!(out, "* frequency (Hz), deviation (dB)")?;
    for (hz, db) in centres.iter().zip(correction) {
        if have(*db) {
            writeln!(out, "{hz:.1} {:.2}", -(*db - at))?;
        }
    }
    Ok(())
}

/// A calibration read from a file.
pub struct Parsed {
    pub count: usize,
    pub absolute: bool,
}

/// `[-+]digits[.digits]`.
pub fn parse_number(text: &str) -> Option<f32> {
    let (negative, digits) = match text.as_bytes().first()? {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty() || !whole.bytes().chain(fraction.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value = 0.0f32;
    for b in whole.bytes() {
        value = value * 10.0 + (b - b'0') as f32;
    }
    let mut scale = 0.1f32;
    for b in fraction.bytes() {
        value += (b - b'0') as f32 * scale;
        scale *= 0.1;
    }
    Some(if negative { -value } else { value })
}

/// Reads a fos file or a REW-style one into `hz` and `db` as corrections (a REW deviation is negated); NaN-free points only.
pub fn parse(text: &str, hz: &mut [f32], db: &mut [f32]) -> Parsed {
    let capacity = hz.len().min(db.len());
    let (mut count, mut absolute, mut rew) = (0, false, false);
    for line in text.lines() {
        let line = line.trim();
        if let Some(comment) = line.strip_prefix('#') {
            if let Some(value) = comment.trim().strip_prefix("absolute ") {
                absolute = value.trim() == "yes";
            }
            continue;
        }
        if line.starts_with('*') {
            rew = true;
            continue;
        }
        let mut fields = line.split([' ', '\t', ',', ';']).filter(|f| !f.is_empty());
        let (Some(first), Some(second)) = (fields.next(), fields.next()) else { continue };
        let (Some(f), Some(d)) = (parse_number(first), parse_number(second)) else { continue };
        if count < capacity && f > 0.0 {
            hz[count] = f;
            db[count] = if rew { -d } else { d };
            count += 1;
        }
    }
    Parsed { count, absolute: absolute && !rew }
}

/// `points` (ascending Hz) read at `centres`, linear in log frequency, held flat beyond the ends.
pub fn interpolate(point_hz: &[f32], point_db: &[f32], centres: &[f32], out: &mut [f32]) {
    for (i, &hz) in centres.iter().enumerate() {
        out[i] = f32::NAN;
        let n = point_hz.len().min(point_db.len());
        if n == 0 {
            continue;
        }
        out[i] = if hz <= point_hz[0] {
            point_db[0]
        } else if hz >= point_hz[n - 1] {
            point_db[n - 1]
        } else {
            let upper = (1..n).find(|&k| point_hz[k] >= hz).unwrap_or(n - 1);
            let (a, b) = (log10(point_hz[upper - 1]), log10(point_hz[upper]));
            let t = if b > a { (log10(hz) - a) / (b - a) } else { 0.0 };
            point_db[upper - 1] + t * (point_db[upper] - point_db[upper - 1])
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::string::String;
    use std::vec::Vec;

    fn bands() -> Vec<f32> {
        (0..31).map(|i| 20.0 * 10f32.powf(i as f32 / 10.0)).collect()
    }

    /// A made-up mic: 3 dB down at the bottom, 4 dB up near 10 kHz.
    fn response(hz: f32) -> f32 {
        -3.0 * (100.0 / hz).min(1.0) + 4.0 * (hz / 10000.0).min(1.0)
    }

    fn inputs<'a>(centres: &'a [f32], measured: &'a [f32]) -> Inputs<'a> {
        Inputs { centres, measured, reference: None, reference_correction: None, tone: None, tone_spl: 94.0, target_band_db: 79.0 }
    }

    #[test]
    fn pink_total_is_spread_over_the_bands() {
        assert!((pink_band_level(94.0, 31) - 79.09).abs() < 0.01);
        assert_eq!(pink_band_level(60.0, 1), 60.0);
    }

    #[test]
    fn coupler_undoes_the_mic_response() {
        let centres = bands();
        let measured: Vec<f32> = centres.iter().map(|&hz| -20.0 + response(hz)).collect();
        let mut out = std::vec![0.0; 31];
        let outcome = correction(Method::Coupler, &inputs(&centres, &measured), &mut out).unwrap();
        assert!(outcome.absolute);
        for (i, &hz) in centres.iter().enumerate() {
            // spl = level + correction must be the same 79 dB in every band
            assert!((measured[i] + out[i] - 79.0).abs() < 1e-3, "{hz}");
        }
    }

    #[test]
    fn reference_is_relative_without_a_tone_or_reference_file() {
        let centres = bands();
        let truth: Vec<f32> = centres.iter().map(|_| 60.0).collect();
        let reference: Vec<f32> = truth.iter().map(|t| t - 30.0).collect(); // the reference mic's own gain
        let measured: Vec<f32> = centres.iter().zip(&truth).map(|(&hz, t)| t - 41.0 + response(hz)).collect(); // other gain
        let mut inp = inputs(&centres, &measured);
        inp.reference = Some(&reference);
        let mut out = std::vec![0.0; 31];
        let outcome = correction(Method::Reference, &inp, &mut out).unwrap();
        assert!(!outcome.absolute);
        let band = reference_band(&centres).unwrap();
        assert!(out[band].abs() < 1e-4);
        for i in 0..31 {
            assert!((out[i] + response(centres[i]) - response(centres[band])).abs() < 1e-3);
        }
    }

    #[test]
    fn reference_with_its_own_correction_is_absolute() {
        let centres = bands();
        let reference = std::vec![-30.0; 31];
        let own = std::vec![90.0; 31]; // spl = -30 + 90 = 60
        let measured = std::vec![-41.0; 31];
        let mut inp = inputs(&centres, &measured);
        inp.reference = Some(&reference);
        inp.reference_correction = Some(&own);
        let mut out = std::vec![0.0; 31];
        assert!(correction(Method::Reference, &inp, &mut out).unwrap().absolute);
        assert!((measured[5] + out[5] - 60.0).abs() < 1e-4);
    }

    #[test]
    fn calibrator_sets_a_flat_sensitivity() {
        let centres = bands();
        let measured = std::vec![f32::NAN; 31];
        let tone: Vec<f32> = (0..31).map(|i| if centres[i] == centres[reference_band(&centres).unwrap()] { -26.0 } else { f32::NAN }).collect();
        let mut inp = inputs(&centres, &measured);
        inp.tone = Some(&tone);
        let mut out = std::vec![0.0; 31];
        assert!(correction(Method::Calibrator, &inp, &mut out).unwrap().absolute);
        assert!(out.iter().all(|&v| (v - 120.0).abs() < 1e-4));
        inp.tone = None;
        assert!(correction(Method::Calibrator, &inp, &mut out).is_err());
    }

    #[test]
    fn a_tone_makes_a_reference_curve_absolute() {
        let centres = bands();
        let band = reference_band(&centres).unwrap();
        let reference = std::vec![-30.0; 31];
        let measured: Vec<f32> = centres.iter().map(|&hz| -41.0 + response(hz)).collect();
        let mut tone = std::vec![f32::NAN; 31];
        tone[band] = -26.0 + response(centres[band]);
        let mut inp = inputs(&centres, &measured);
        inp.reference = Some(&reference);
        inp.tone = Some(&tone);
        let mut out = std::vec![0.0; 31];
        assert!(correction(Method::Reference, &inp, &mut out).unwrap().absolute);
        assert!((tone[band] + out[band] - 94.0).abs() < 1e-3);
    }

    #[test]
    fn missing_captures_are_named() {
        let centres = bands();
        let measured = std::vec![f32::NAN; 31];
        let mut out = std::vec![0.0; 31];
        assert!(correction(Method::Coupler, &inputs(&centres, &measured), &mut out).is_err());
        let measured = std::vec![-30.0; 31];
        assert_eq!(correction(Method::Reference, &inputs(&centres, &measured), &mut out).err(), Some("capture the reference mic first"));
    }

    fn written(rew: bool) -> String {
        let centres = bands();
        let correction: Vec<f32> = centres.iter().map(|&hz| 80.0 - response(hz)).collect();
        let header = Header { name: "m1", stamp: "t", method: Method::Coupler, rate: 48000, outcome: Outcome { absolute: true } };
        let mut text = String::new();
        if rew { write_rew(&mut text, &header, &centres, &correction).unwrap() } else { write_fos(&mut text, &header, &centres, &correction).unwrap() }
        text
    }

    #[test]
    fn fos_file_round_trips() {
        let text = written(false);
        let (mut hz, mut db) = ([0.0; MAX_POINTS], [0.0; MAX_POINTS]);
        let parsed = parse(&text, &mut hz, &mut db);
        assert_eq!(parsed.count, 31);
        assert!(parsed.absolute);
        let centres = bands();
        assert!((hz[10] - centres[10]).abs() < 0.06);
        assert!((db[0] - (80.0 - response(centres[0]))).abs() < 0.006);
    }

    #[test]
    fn rew_file_is_the_inverse_deviation_and_reads_back_as_a_relative_correction() {
        let text = written(true);
        assert!(text.starts_with("* miccal"));
        let (mut hz, mut db) = ([0.0; MAX_POINTS], [0.0; MAX_POINTS]);
        let parsed = parse(&text, &mut hz, &mut db);
        assert_eq!(parsed.count, 31);
        assert!(!parsed.absolute);
        let centres = bands();
        let at = reference_band(&centres).unwrap();
        // the correction relative to 1 kHz: response(1 kHz) - response(f)
        assert!((db[0] - (response(centres[at]) - response(centres[0]))).abs() < 0.02);
    }

    #[test]
    fn interpolation_is_logarithmic_and_holds_the_ends() {
        let mut out = [0.0; 4];
        interpolate(&[100.0, 10000.0], &[0.0, 10.0], &[50.0, 100.0, 1000.0, 20000.0], &mut out);
        assert_eq!(out[0], 0.0);
        assert!((out[2] - 5.0).abs() < 0.01);
        assert_eq!(out[3], 10.0);
        interpolate(&[], &[], &[1000.0], &mut out);
        assert!(out[0].is_nan());
    }

    #[test]
    fn numbers_parse() {
        assert_eq!(parse_number("-12.5"), Some(-12.5));
        assert_eq!(parse_number("1000"), Some(1000.0));
        assert_eq!(parse_number("x"), None);
        assert_eq!(parse_number("-"), None);
    }
}
