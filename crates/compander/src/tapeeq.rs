//! Tape equalisation conversion: playing a tape recorded to one standard on a machine aligned to the other (NAB or IEC, the
//! latter also called CCIR/DIN), and recording for the other standard.
//!
//! A reel-to-reel playback equaliser has the flux-referenced response `H(s) = (1 + 1/(s T1)) / (1 + s T2)`: `T1` is the
//! low-frequency time constant (3180 us for NAB, none for IEC) and `T2` the high-frequency one (the speed decides it). A tape
//! recorded to standard S and played on a machine aligned to M therefore comes out with `H_M / H_S` instead of flat, and the
//! correction `C = H_S / H_M` after the machine puts that right:
//!
//! ```text
//! C(s) = (s + 1/T1s) / (s + 1/T1m)  *  (1 + s T2m) / (1 + s T2s)
//! ```
//!
//! Recording for S on a machine aligned to M needs the inverse, `1/C`, before the machine's record equaliser. IEC has no `T1`
//! (an infinite time constant), which would make the correction an integrator: that corner is taken as `LF_FLOOR_HZ` instead, so the
//! correction levels off below it. The values are the commonly published ones (NAB 3180 us with 50 us at 7.5 and 15 ips, 90 us at
//! 3.75, 17.5 us at 30; IEC 90 us at 3.75, 70 us at 7.5, 35 us at 15, 17.5 us at 30); check them against the standards for
//! critical work.

use crate::First;
use dsp::math;

/// Where an infinite low-frequency time constant is cut off (Hz).
pub const LF_FLOOR_HZ: f32 = 10.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Standard {
    Nab,
    /// IEC / CCIR / DIN.
    Iec,
}

impl Standard {
    pub fn name(self) -> &'static str {
        match self {
            Standard::Nab => "NAB",
            Standard::Iec => "IEC",
        }
    }

    pub fn other(self) -> Standard {
        match self {
            Standard::Nab => Standard::Iec,
            Standard::Iec => Standard::Nab,
        }
    }
}

/// Tape speed in inches per second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speed {
    Ips3_75,
    Ips7_5,
    Ips15,
    Ips30,
}

impl Speed {
    pub fn name(self) -> &'static str {
        match self {
            Speed::Ips3_75 => "3.75",
            Speed::Ips7_5 => "7.5",
            Speed::Ips15 => "15",
            Speed::Ips30 => "30",
        }
    }

    pub fn next(self) -> Speed {
        match self {
            Speed::Ips3_75 => Speed::Ips7_5,
            Speed::Ips7_5 => Speed::Ips15,
            Speed::Ips15 => Speed::Ips30,
            Speed::Ips30 => Speed::Ips3_75,
        }
    }

    pub fn from_name(name: &str) -> Option<Speed> {
        [Speed::Ips3_75, Speed::Ips7_5, Speed::Ips15, Speed::Ips30].into_iter().find(|s| s.name() == name)
    }
}

/// (T1, T2) in microseconds; `None` is an infinite T1.
pub fn time_constants(standard: Standard, speed: Speed) -> (Option<f32>, f32) {
    let t2 = match (standard, speed) {
        (Standard::Nab, Speed::Ips3_75) | (Standard::Iec, Speed::Ips3_75) => 90.0,
        (Standard::Nab, Speed::Ips7_5) => 50.0,
        (Standard::Nab, Speed::Ips15) => 50.0,
        (Standard::Iec, Speed::Ips7_5) => 70.0,
        (Standard::Iec, Speed::Ips15) => 35.0,
        (_, Speed::Ips30) => 17.5,
    };
    (if standard == Standard::Nab { Some(3180.0) } else { None }, t2)
}

fn t1_hz(t1_us: Option<f32>) -> f32 {
    t1_us.map_or(LF_FLOOR_HZ, |t| 1e6 / (2.0 * core::f32::consts::PI * t))
}

fn t2_hz(t2_us: f32) -> f32 {
    1e6 / (2.0 * core::f32::consts::PI * t2_us)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// After the machine's playback output.
    Playback,
    /// Before the machine's record input.
    Record,
}

/// One channel of the conversion (a no-op when the tape standard is the machine's).
#[derive(Clone, Copy)]
pub struct Conversion {
    lf: First,
    hf: First,
    gain: f32,
    active: bool,
}

impl Conversion {
    pub fn new(rate: f32, machine: Standard, tape: Standard, speed: Speed, direction: Direction) -> Conversion {
        let (t1m, t2m) = time_constants(machine, speed);
        let (t1s, t2s) = time_constants(tape, speed);
        // Playback: C = (s + as)/(s + am) * (1 + s T2m)/(1 + s T2s); record uses the inverse of every factor.
        let (a_s, a_m) = (t1_hz(t1s), t1_hz(t1m));
        let (z_lf, p_lf, z_hf, p_hf) = match direction {
            Direction::Playback => (a_s, a_m, t2_hz(t2m), t2_hz(t2s)),
            Direction::Record => (a_m, a_s, t2_hz(t2s), t2_hz(t2m)),
        };
        Conversion {
            lf: First::shelf(z_lf, p_lf, rate),
            hf: First::shelf(z_hf, p_hf, rate),
            gain: z_lf / p_lf,
            active: machine != tape,
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn process(&mut self, x: f32) -> f32 {
        if !self.active {
            return x;
        }
        self.hf.run(self.lf.run(x) * self.gain)
    }
}

/// The playback correction's ideal (analogue) response at `hz`, dB: what a tape of standard `tape` needs on a machine aligned to `machine`.
pub fn playback_response_db(machine: Standard, tape: Standard, speed: Speed, hz: f32) -> f32 {
    let (t1m, t2m) = time_constants(machine, speed);
    let (t1s, t2s) = time_constants(tape, speed);
    let (a_s, a_m) = (t1_hz(t1s), t1_hz(t1m));
    let f = hz;
    let corner = |zero: f32, pole: f32| math::sqrt((f * f + zero * zero) / (f * f + pole * pole));
    let (fs, fm) = (t2_hz(t2s), t2_hz(t2m));
    let hf = math::sqrt((1.0 + (f / fm) * (f / fm)) / (1.0 + (f / fs) * (f / fs)));
    20.0 * math::log10(corner(a_s, a_m) * hf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48_000.0;

    fn measure(mut conversion: Conversion, hz: f32) -> f32 {
        let n = 48_000;
        let (mut sum, mut count) = (0.0f64, 0);
        for i in 0..n {
            let x = 0.1 * (2.0 * core::f32::consts::PI * hz * i as f32 / RATE).sin();
            let y = conversion.process(x);
            if i >= n / 2 {
                sum += (y * y) as f64;
                count += 1;
            }
        }
        (10.0 * (sum / count as f64 / (0.1f64 * 0.1 / 2.0)).log10()) as f32
    }

    #[test]
    fn same_standard_is_untouched() {
        let mut c = Conversion::new(RATE, Standard::Nab, Standard::Nab, Speed::Ips15, Direction::Playback);
        assert!(!c.active());
        assert_eq!(c.process(0.3), 0.3);
    }

    #[test]
    fn a_nab_tape_on_an_iec_machine_is_corrected_as_the_standards_say() {
        // At 15 ips NAB is 3180/50 us, IEC 35 us: the machine lacks the NAB bass lift (about 50 Hz corner) and cuts treble too little.
        for (hz, tolerance) in [(50.0, 0.3), (200.0, 0.2), (1000.0, 0.2), (4000.0, 0.3), (10_000.0, 0.8)] {
            let want = playback_response_db(Standard::Iec, Standard::Nab, Speed::Ips15, hz);
            let got = measure(Conversion::new(RATE, Standard::Iec, Standard::Nab, Speed::Ips15, Direction::Playback), hz);
            println!("{hz} Hz: {got:+.2} dB (ideal {want:+.2})");
            assert!((got - want).abs() < tolerance, "{hz} Hz: {got} vs {want}");
        }
        // Treble: 50 us tape on a 35 us machine needs a cut, bass a lift.
        assert!(playback_response_db(Standard::Iec, Standard::Nab, Speed::Ips15, 10_000.0) < -2.0);
        assert!(playback_response_db(Standard::Iec, Standard::Nab, Speed::Ips15, 20.0) > 6.0);
    }

    #[test]
    fn the_reverse_is_the_opposite() {
        for hz in [30.0, 300.0, 3000.0, 9000.0] {
            let there = playback_response_db(Standard::Iec, Standard::Nab, Speed::Ips7_5, hz);
            let back = playback_response_db(Standard::Nab, Standard::Iec, Speed::Ips7_5, hz);
            assert!((there + back).abs() < 1e-3, "{hz} Hz");
        }
    }

    #[test]
    fn recording_for_the_other_standard_undoes_playback() {
        let mut record = Conversion::new(RATE, Standard::Iec, Standard::Nab, Speed::Ips15, Direction::Record);
        let mut play = Conversion::new(RATE, Standard::Iec, Standard::Nab, Speed::Ips15, Direction::Playback);
        for hz in [40.0, 1000.0, 8000.0] {
            let mut worst = 0.0f32;
            for i in 0..48_000 {
                let x = 0.2 * (2.0 * core::f32::consts::PI * hz * i as f32 / RATE).sin();
                let y = play.process(record.process(x));
                if i > 24_000 {
                    worst = worst.max((y - x).abs());
                }
            }
            assert!(worst < 1e-3, "{hz} Hz: {worst}");
        }
    }
}
