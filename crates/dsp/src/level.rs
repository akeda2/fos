//! Level readings for meters, in dB: a sample-peak detector with a controlled fall-back, and a peak-hold marker that works on
//! any dB reading (so it also holds a PPM reading). The quasi-peak (Nordic PPM) detector itself is `audio_wire::meter`.

use crate::math::{log10, pow10};

/// The lowest level reported (silence).
pub const FLOOR_DB: f32 = -120.0;

/// Sample peak in dBFS: `feed` takes the samples as they arrive, `read` (once per display frame) gives the highest peak since
/// the last read, falling back at most `fall_db_per_s`. Latches `clipped` when a sample reaches `clip_level` (linear).
#[derive(Clone, Copy)]
pub struct SamplePeak {
    block_max: f32,
    level_db: f32,
    pub fall_db_per_s: f32,
    pub clip_level: f32,
    pub clipped: bool,
}

impl SamplePeak {
    pub const fn new() -> Self {
        Self { block_max: 0.0, level_db: FLOOR_DB, fall_db_per_s: 20.0, clip_level: 0.9995, clipped: false }
    }

    pub fn feed(&mut self, samples: &[f32]) {
        for &s in samples {
            let a = if s < 0.0 { -s } else { s };
            if a > self.block_max {
                self.block_max = a;
            }
        }
        if self.block_max >= self.clip_level {
            self.clipped = true;
        }
    }

    pub fn read(&mut self, dt_s: f32) -> f32 {
        let fallen = self.level_db - self.fall_db_per_s * dt_s;
        let new_db = if self.block_max > 1e-6 { 20.0 * log10(self.block_max) } else { FLOOR_DB };
        self.level_db = if new_db > fallen { new_db } else if fallen > FLOOR_DB { fallen } else { FLOOR_DB };
        self.block_max = 0.0;
        self.level_db
    }

    pub fn reset(&mut self) {
        *self = Self { fall_db_per_s: self.fall_db_per_s, clip_level: self.clip_level, ..Self::new() };
    }
}

impl Default for SamplePeak {
    fn default() -> Self {
        Self::new()
    }
}

/// A peak-hold marker: follows a reading up at once, stays `hold_s`, then falls at `fall_db_per_s` (never below the reading).
#[derive(Clone, Copy)]
pub struct PeakHold {
    pub value_db: f32,
    timer_s: f32,
    pub hold_s: f32,
    pub fall_db_per_s: f32,
}

impl PeakHold {
    pub const fn new() -> Self {
        Self { value_db: FLOOR_DB, timer_s: 0.0, hold_s: 2.0, fall_db_per_s: 20.0 }
    }

    pub fn update(&mut self, reading_db: f32, dt_s: f32) -> f32 {
        if reading_db >= self.value_db {
            self.value_db = reading_db;
            self.timer_s = self.hold_s;
        } else if self.timer_s > 0.0 {
            self.timer_s -= dt_s;
        } else {
            let fallen = self.value_db - self.fall_db_per_s * dt_s;
            self.value_db = if fallen > reading_db { fallen } else { reading_db };
        }
        self.value_db
    }

    pub fn reset(&mut self) {
        self.value_db = FLOOR_DB;
        self.timer_s = 0.0;
    }
}

impl Default for PeakHold {
    fn default() -> Self {
        Self::new()
    }
}

/// dB of a linear amplitude, floored at `FLOOR_DB`.
pub fn amplitude_db(amplitude: f32) -> f32 {
    if amplitude > 1e-6 { 20.0 * log10(amplitude) } else { FLOOR_DB }
}

/// Amplitude of a dB level.
pub fn db_amplitude(db: f32) -> f32 {
    pow10(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peak_reads_falls_and_clips() {
        let mut p = SamplePeak::new();
        p.feed(&[0.0, -0.5, 0.25]);
        assert!((p.read(0.05) + 6.02).abs() < 0.05);
        assert!(!p.clipped);
        let fallen = p.read(0.5);
        assert!((fallen - (-6.02 - 10.0)).abs() < 0.05);
        p.feed(&[1.0]);
        assert!(p.clipped);
    }

    #[test]
    fn hold_stays_then_falls() {
        let mut h = PeakHold::new();
        assert_eq!(h.update(-10.0, 0.05), -10.0);
        assert_eq!(h.update(-30.0, 1.0), -10.0);
        assert_eq!(h.update(-30.0, 1.5), -10.0);
        assert!(h.update(-30.0, 0.5) < -10.0);
        for _ in 0..100 {
            h.update(-30.0, 0.5);
        }
        assert_eq!(h.value_db, -30.0);
    }
}
