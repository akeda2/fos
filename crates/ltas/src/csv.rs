//! The analyser's result as CSV text, for a spreadsheet or a script: a few `#` comment lines with the settings, then one
//! row per band (or FFT bin) with its centre and edge frequencies and the four views in dB.

use crate::{Analyzer, Mode};
use core::fmt::{Error, Write};

/// Writes the CSV into `out`. `stamp` is free text for the first line (the tape passes the date and time). `scratch` needs
/// room for four views of `analyzer.outputs()` values. Fails only if `out` does.
pub fn write<const N: usize, W: Write>(analyzer: &mut Analyzer<N>, stamp: &str, scratch: &mut [f32], out: &mut W) -> Result<(), Error> {
    let count = analyzer.outputs();
    if scratch.len() < 4 * count {
        return Err(Error);
    }
    let config = *analyzer.config();
    writeln!(out, "# ltas {stamp}")?;
    writeln!(
        out,
        "# rate {} Hz, fft {}, hop {}, window {}, resolution {}, weighting {}, calibration {:.1} dB, tau {:.3} s, frames {}",
        config.sample_rate,
        config.n,
        config.hop,
        config.window.name(),
        config.resolution.name(),
        config.weighting.name(),
        config.calibration_db,
        config.tau_s,
        analyzer.frames()
    )?;
    writeln!(out, "# levels in dB re a full-scale sine; resolved = 0 for a band narrower than two FFT bins")?;
    writeln!(out, "centre_hz,low_hz,high_hz,resolved,live_db,exp_db,ltas_db,peak_db")?;
    let (live, rest) = scratch.split_at_mut(count);
    let (exp, rest) = rest.split_at_mut(count);
    let (ltas, rest) = rest.split_at_mut(count);
    let peak = &mut rest[..count];
    analyzer.levels_db(Mode::Live, live);
    analyzer.levels_db(Mode::Exp, exp);
    analyzer.levels_db(Mode::Linear, ltas);
    analyzer.levels_db(Mode::Peak, peak);
    for i in 0..count {
        let (low, high) = analyzer.edges_hz(i);
        writeln!(
            out,
            "{:.2},{:.2},{:.2},{},{:.2},{:.2},{:.2},{:.2}",
            analyzer.centre_hz(i),
            low,
            high,
            analyzer.band_is_resolved(i) as u8,
            live[i],
            exp[i],
            ltas[i],
            peak[i]
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn rows_match_the_bands_and_carry_the_levels() {
        let mut analyzer = Box::new(Analyzer::<8192>::new());
        assert!(analyzer.configure(Config::new(48000)));
        let samples: Vec<f32> = (0..48000).map(|t| 0.5 * (2.0 * std::f64::consts::PI * 1000.0 * t as f64 / 48000.0).sin() as f32).collect();
        analyzer.push(&samples);
        let mut text = String::new();
        let mut scratch = vec![0.0; 4 * analyzer.outputs()];
        write(&mut *analyzer, "test", &mut scratch, &mut text).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4 + analyzer.outputs());
        assert_eq!(lines[3], "centre_hz,low_hz,high_hz,resolved,live_db,exp_db,ltas_db,peak_db");
        let best = lines[4..].iter().map(|row| row.split(',').collect::<Vec<_>>()).max_by(|a, b| a[6].parse::<f32>().unwrap().total_cmp(&b[6].parse::<f32>().unwrap())).unwrap();
        assert!((best[0].parse::<f32>().unwrap() / 1000.0 - 1.0).abs() < 0.08);
        assert!((best[6].parse::<f32>().unwrap() + 6.02).abs() < 0.8);
        assert_eq!(best.len(), 8);
    }

    #[test]
    fn too_little_scratch_is_an_error() {
        let mut analyzer = Box::new(Analyzer::<8192>::new());
        assert!(analyzer.configure(Config::new(48000)));
        let mut text = String::new();
        assert!(write(&mut *analyzer, "", &mut [0.0; 3], &mut text).is_err());
    }
}
