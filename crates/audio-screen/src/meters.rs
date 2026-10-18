//! The level readings of one input channel for a meter bar: a sample-peak detector, a Nordic PPM and a peak hold for each, so
//! a tape can show either scale (or both) from the same samples. Feed samples as they arrive; `read` once per display frame.

use audio_wire::meter::{self, Coefficients, Ppm};
use dsp::level::{FLOOR_DB, PeakHold, SamplePeak};
use gfx::meter::Scale;

/// Which reading a meter shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Quasi-peak, Nordic scale (0 = TEST = -18 dBFS).
    Nordic,
    /// Sample peak, dBFS.
    Dbfs,
}

impl Kind {
    pub fn scale(self) -> &'static Scale {
        match self {
            Kind::Nordic => &Scale::NORDIC,
            Kind::Dbfs => &Scale::DBFS,
        }
    }

    pub fn next(self) -> Kind {
        match self {
            Kind::Nordic => Kind::Dbfs,
            Kind::Dbfs => Kind::Nordic,
        }
    }
}

/// What a bar shows, in the units of its scale.
#[derive(Clone, Copy)]
pub struct Reading {
    pub level: f32,
    pub hold: f32,
    pub clipped: bool,
}

pub struct Channel {
    peak: SamplePeak,
    ppm: Ppm,
    coefficients: Coefficients,
    hold_nordic: PeakHold,
    hold_dbfs: PeakHold,
}

impl Channel {
    pub fn new(rate: u32) -> Self {
        Self { peak: SamplePeak::new(), ppm: Ppm::new(), coefficients: Coefficients::for_rate(rate), hold_nordic: PeakHold::new(), hold_dbfs: PeakHold::new() }
    }

    pub fn feed(&mut self, samples: &[f32]) {
        self.peak.feed(samples);
        for &sample in samples {
            self.ppm.feed(meter::magnitude(sample), &self.coefficients);
        }
    }

    /// Both readings advance (so switching scale shows a current value); returns the one asked for. `dt_s` is the time since
    /// the last call.
    pub fn read(&mut self, kind: Kind, dt_s: f32) -> Reading {
        let dbfs = self.peak.read(dt_s);
        let ppm_dbfs = match self.ppm.take_max() {
            0 => FLOOR_DB,
            level => meter::db_tenths(level) as f32 / 10.0,
        };
        let hold_dbfs = self.hold_dbfs.update(dbfs, dt_s);
        let hold_ppm = self.hold_nordic.update(ppm_dbfs, dt_s);
        let clipped = self.peak.clipped;
        match kind {
            Kind::Dbfs => Reading { level: dbfs, hold: hold_dbfs, clipped },
            Kind::Nordic => Reading { level: ppm_dbfs - Scale::NORDIC.dbfs_at_zero, hold: hold_ppm - Scale::NORDIC.dbfs_at_zero, clipped },
        }
    }

    /// Clears the clip lamp and the holds.
    pub fn reset(&mut self) {
        self.peak.reset();
        self.hold_nordic.reset();
        self.hold_dbfs.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_half_scale_burst_reads_on_both_scales() {
        let mut channel = Channel::new(48_000);
        let burst = [0.5f32; 480];
        channel.feed(&burst);
        let dbfs = channel.read(Kind::Dbfs, 0.05);
        assert!((dbfs.level + 6.02).abs() < 0.1 && !dbfs.clipped);
        let mut nordic = Channel::new(48_000);
        nordic.feed(&burst);
        let ppm = nordic.read(Kind::Nordic, 0.05);
        // 10 ms of -6 dBFS reads about 1 dB low: about -7 dBFS, which is +11 on the Nordic scale
        assert!(ppm.level > 8.0 && ppm.level < 12.5, "{}", ppm.level);
    }

    #[test]
    fn full_scale_latches_the_clip_lamp_until_reset() {
        let mut channel = Channel::new(48_000);
        channel.feed(&[1.0]);
        assert!(channel.read(Kind::Dbfs, 0.05).clipped);
        assert!(channel.read(Kind::Dbfs, 0.05).clipped);
        channel.reset();
        assert!(!channel.read(Kind::Dbfs, 0.05).clipped);
    }
}
