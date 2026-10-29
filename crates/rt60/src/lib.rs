//! Room acoustics from an impulse response: RT60 (EDT, T20, T30), clarity and definition per octave or 1/3-octave band, from a
//! balloon pop (`Capture`) or an exponential sweep (`Sweep`). No hardware, no allocation; the `frt60` tape does the audio, the
//! drawing and the files.
//!
//! Flow: capture a response, `decay::direct_sound` finds where it starts, then for each band `measure` copies the response
//! into a work buffer, filters it (`filter::band`, zero phase) and `decay::analyse` turns it into a decay curve and a `Report`.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod capture;
pub mod decay;
pub mod files;
pub mod filter;
pub mod sweep;

pub use capture::Capture;
pub use decay::{Report, direct_sound};
pub use sweep::Sweep;

/// The most bands (1/3-octave 50 Hz-10 kHz) plus the overall figure.
pub const MAX_BANDS: usize = 25;

/// A band layout: octave (63 Hz-8 kHz, 8 bands) or 1/3 octave (50 Hz-10 kHz, 24 bands).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Band {
    Octave,
    Third,
}

impl Band {
    pub fn name(self) -> &'static str {
        match self {
            Band::Octave => "octave",
            Band::Third => "1/3 octave",
        }
    }

    pub fn from_name(name: &str) -> Option<Band> {
        match name {
            "octave" | "1" => Some(Band::Octave),
            "third" | "1/3" | "3" => Some(Band::Third),
            _ => None,
        }
    }

    pub fn next(self) -> Band {
        match self {
            Band::Octave => Band::Third,
            Band::Third => Band::Octave,
        }
    }

    fn per_octave(self) -> u32 {
        match self {
            Band::Octave => 1,
            Band::Third => 3,
        }
    }

    /// Band indices counted from 1 kHz: octave -4..=3 (63 Hz-8 kHz), third -13..=10 (50 Hz-10 kHz).
    fn first(self) -> i32 {
        match self {
            Band::Octave => -4,
            Band::Third => -13,
        }
    }

    pub fn count(self) -> usize {
        match self {
            Band::Octave => 8,
            Band::Third => 24,
        }
    }

    /// (lower edge, centre, upper edge) in Hz of band `i` (0 is the lowest).
    pub fn edges(self, i: usize) -> (f32, f32, f32) {
        ltas::bands::band_edges(self.per_octave(), self.first() + i as i32)
    }
}

/// Measures one band of `ir` (which must start at the direct sound) into `work` (at least `ir.len()` long), where the decay
/// curve in dB stays for drawing. `edges` None is the whole response, unfiltered.
pub fn measure(ir: &[f32], rate: u32, edges: Option<(f32, f32, f32)>, work: &mut [f32]) -> Report {
    let work = &mut work[..ir.len()];
    work.copy_from_slice(ir);
    let (centre, smooth) = match edges {
        Some((low, centre, high)) => {
            filter::band(work, rate, low, high);
            // A few periods of the band's centre, but never under 10 ms: a low band's envelope would flutter below the noise.
            (centre, ((3.0 * rate as f32 / centre) as usize).max(rate as usize / 100))
        }
        None => (0.0, rate as usize / 100),
    };
    decay::analyse(work, rate, smooth, centre)
}

/// Time steps in a waterfall row.
pub const WATERFALL_STEPS: usize = 96;

/// The decay of every band side by side: x = band, y = time after the direct sound, value = level in dB (0 at the start).
/// It is an energy decay relief: each cell is the Schroeder decay curve of that band (energy still to come, the same curve the
/// reverberation times are fitted to), so a ridge that stays high is a ringing room mode and the slope across bands is the
/// RT60-against-frequency curve. `FLOOR_DB` marks time beyond the point where the band sank into the noise.
pub struct Waterfall {
    pub span_s: f32,
    pub bands: usize,
    pub cells: [[f32; WATERFALL_STEPS]; MAX_BANDS],
}

impl Waterfall {
    pub const fn new() -> Waterfall {
        Waterfall { span_s: 1.0, bands: 0, cells: [[decay::FLOOR_DB; WATERFALL_STEPS]; MAX_BANDS] }
    }

    pub fn clear(&mut self, bands: usize, span_s: f32) {
        self.bands = bands.min(MAX_BANDS);
        self.span_s = span_s;
        for row in self.cells.iter_mut() {
            row.fill(decay::FLOOR_DB);
        }
    }

    /// Stores band `band` from its decay curve `curve` (what `measure` leaves in `work`).
    pub fn set(&mut self, band: usize, curve: &[f32], rate: u32) {
        if band >= MAX_BANDS {
            return;
        }
        for (step, cell) in self.cells[band].iter_mut().enumerate() {
            let at = (step as f32 * self.span_s / (WATERFALL_STEPS - 1) as f32 * rate as f32) as usize;
            *cell = curve.get(at).copied().unwrap_or(decay::FLOOR_DB);
        }
    }
}

/// A good time span for the waterfall: about 1.5 times the longest valid T20 (or EDT), clamped to 0.3..6 s.
pub fn waterfall_span(reports: &[Report]) -> f32 {
    let mut longest = 0.0f32;
    for r in reports {
        for t in [r.t20, r.edt] {
            if t.is_finite() && t > longest {
                longest = t;
            }
        }
    }
    (longest * 1.5).clamp(0.3, 6.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dsp::fft::Plan;
    use std::vec;
    use std::vec::Vec;

    const RATE: u32 = 48000;

    struct Noise(u64);
    impl Noise {
        /// Roughly Gaussian, unit variance: the sum of four uniforms.
        fn next(&mut self) -> f32 {
            let mut sum = 0.0;
            for _ in 0..4 {
                self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                sum += (self.0 >> 40) as f32 / (1u64 << 24) as f32 - 0.5;
            }
            sum * 1.732
        }
    }

    /// A room: noise whose amplitude falls 60 dB in `t60` seconds from a direct-sound click, plus a steady floor `floor_db` below
    /// the start.
    fn room(t60: f32, seconds: f32, floor_db: f32) -> Vec<f32> {
        let mut noise = Noise(7);
        let n = (seconds * RATE as f32) as usize;
        let floor = 10f32.powf(floor_db / 20.0);
        (0..n)
            .map(|i| {
                let t = i as f32 / RATE as f32;
                let tail = if i == 0 { 8.0 } else { noise.next() * 10f32.powf(-3.0 * t / t60) };
                tail + noise.next() * floor
            })
            .collect()
    }

    fn near(value: f32, expected: f32, tolerance: f32) -> bool {
        (value / expected - 1.0).abs() <= tolerance
    }

    #[test]
    fn broadband_decay_is_recovered() {
        let ir = room(1.0, 4.0, -80.0);
        let mut work = vec![0.0; ir.len()];
        let r = measure(&ir[direct_sound(&ir)..], RATE, None, &mut work);
        assert!(near(r.edt, 1.0, 0.1) && near(r.t20, 1.0, 0.06) && near(r.t30, 1.0, 0.05), "{r:?}");
        assert!(r.dynamic_db > 55.0, "{r:?}");
    }

    #[test]
    fn waterfall_rows_fall_at_the_decay_rate() {
        let ir = room(1.0, 4.0, -90.0);
        let mut work = vec![0.0; ir.len()];
        let mut wf = Waterfall::new();
        wf.clear(1, 1.0);
        measure(&ir, RATE, None, &mut work);
        wf.set(0, &work, RATE);
        let row = &wf.cells[0];
        assert!(row[0] > -1.0);
        let last = row[WATERFALL_STEPS - 1];
        assert!(last > -66.0 && last < -54.0, "{last}");
        assert!(row.windows(2).all(|w| w[1] <= w[0] + 0.01));
    }

    #[test]
    fn every_octave_band_gets_the_same_time() {
        let ir = room(1.0, 4.0, -80.0);
        let mut work = vec![0.0; ir.len()];
        for i in 0..Band::Octave.count() {
            let r = measure(&ir[direct_sound(&ir)..], RATE, Some(Band::Octave.edges(i)), &mut work);
            let tolerance = if i < 2 { 0.2 } else { 0.1 };
            assert!(near(r.t20, 1.0, tolerance) && near(r.t30, 1.0, tolerance), "band {i} {r:?}");
        }
    }

    #[test]
    fn third_octave_layout() {
        let near_hz = |a: f32, b: f32| (a / b - 1.0).abs() < 0.01;
        assert!(near_hz(Band::Third.edges(0).1, 50.1) && near_hz(Band::Third.edges(23).1, 10000.0));
        assert!(near_hz(Band::Octave.edges(0).1, 63.1) && near_hz(Band::Octave.edges(7).1, 7943.0));
    }

    #[test]
    fn a_noise_floor_does_not_lengthen_the_decay() {
        // 0.5 s room with a floor only 50 dB down: uncompensated integration would read long.
        let ir = room(0.5, 3.0, -50.0);
        let mut work = vec![0.0; ir.len()];
        let r = measure(&ir[direct_sound(&ir)..], RATE, None, &mut work);
        assert!(near(r.t20, 0.5, 0.1), "{r:?}");
        assert!(r.dynamic_db < 55.0 && r.used_s < 2.5, "{r:?}");
    }

    #[test]
    fn a_capture_too_short_for_t30_says_so() {
        // Only 35 dB of dynamic range: T20 can be read, T30 cannot.
        let ir = room(0.6, 3.0, -38.0);
        let mut work = vec![0.0; ir.len()];
        let r = measure(&ir[direct_sound(&ir)..], RATE, None, &mut work);
        assert!(r.t30.is_nan() && near(r.t20, 0.6, 0.1), "{r:?}");
    }

    #[test]
    fn definition_and_clarity_follow_the_decay() {
        // Energy falls as exp(-13.8155 t / T60): half of it arrives in the first 50 ms for T60 = 1 s (without the click).
        let mut ir = room(1.0, 4.0, -80.0);
        ir[0] = 0.0;
        let mut work = vec![0.0; ir.len()];
        let r = measure(&ir[1..], RATE, None, &mut work);
        assert!((r.d50 - 0.5).abs() < 0.06, "{r:?}");
        assert!(r.c50.abs() < 1.0 && (r.c80 - 10.0 * (0.6849f32 / 0.3151).log10()).abs() < 1.2, "{r:?}");
    }

    #[test]
    fn the_sweep_gives_the_room_back() {
        std::thread::Builder::new()
            .stack_size(96 << 20)
            .spawn(|| {
                const N: usize = 1 << 18;
                let mut plan = std::boxed::Box::new(Plan::<N>::empty());
                assert!(plan.init(N));
                let sweep = Sweep { rate: RATE, f1: 20.0, f2: 20000.0, samples: 3 * RATE as usize };
                let room = room(0.8, 1.6, -90.0);
                let mut reference = vec![0.0f32; N];
                sweep.fill(&mut reference[..sweep.samples], 0.5);
                // recording = sweep (*) room, circular, delayed by 300 samples of latency; a little noise on top
                let (mut rr, mut ri, mut pr, mut pi) = (vec![0.0; N], vec![0.0; N], reference.clone(), vec![0.0; N]);
                let (mut sr, mut si, mut hr, mut hi) = (reference.clone(), vec![0.0; N], vec![0.0; N], vec![0.0; N]);
                hr[300..300 + room.len()].copy_from_slice(&room);
                plan.forward(&mut sr, &mut si, false);
                plan.forward(&mut hr, &mut hi, false);
                for k in 0..N {
                    let (a, b) = (sr[k] * hr[k] - si[k] * hi[k], sr[k] * hi[k] + si[k] * hr[k]);
                    rr[k] = a;
                    ri[k] = -b; // conjugate: inverse by forward
                }
                plan.forward(&mut rr, &mut ri, false);
                let mut noise = Noise(99);
                for v in rr.iter_mut() {
                    *v = *v / N as f32 + noise.next() * 1e-4;
                }
                ri.fill(0.0);
                deconvolve_check(&plan, &mut rr, &mut ri, &mut pr, &mut pi);
                let start = direct_sound(&rr);
                assert!((start as i64 - 300).abs() <= 2, "the click is at {start}, latency was 300");
                let ir = &rr[start..start + (1.6 * RATE as f32) as usize];
                let mut work = vec![0.0; ir.len()];
                let r = measure(ir, RATE, None, &mut work);
                assert!(near(r.t30, 0.8, 0.1), "{r:?}");
                let r1k = measure(ir, RATE, Some(Band::Octave.edges(4)), &mut work);
                assert!(near(r1k.t20, 0.8, 0.12), "{r1k:?}");
            })
            .unwrap()
            .join()
            .unwrap();
    }

    fn deconvolve_check(plan: &Plan<{ 1 << 18 }>, rr: &mut [f32], ri: &mut [f32], pr: &mut [f32], pi: &mut [f32]) {
        sweep::deconvolve(plan, false, rr, ri, pr, pi);
    }

    #[test]
    fn capture_keeps_the_pre_roll_in_order_and_fills() {
        let mut buf = vec![0.0f32; 20];
        let mut c = Capture::new(&mut buf, 4);
        c.arm(0.5);
        assert!(!c.feed(&[0.1, 0.2, 0.01, 0.02, 0.03, 0.04]));
        assert!(!c.triggered());
        assert!(!c.feed(&[0.9, 0.8, 0.7]));
        assert!(c.triggered());
        assert_eq!(c.recorded()[3], 0.9);
        assert_eq!(&c.recorded()[..3], &[0.02, 0.03, 0.04]);
        assert!(!c.feed(&[0.0; 10]));
        assert!(c.feed(&[0.0; 10]) && c.done());
        assert_eq!(c.recorded().len(), 20);
        assert!(!c.clipped());
    }

    #[test]
    fn files_are_well_formed() {
        let header = wav_header_for_test();
        assert_eq!(&header[..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(header[40..44].try_into().unwrap()), 400);
        let mut text = std::string::String::new();
        let mut reports = [Report::NONE; 2];
        reports[0].centre_hz = 1000.0;
        reports[0].t20 = 1.234;
        reports[1].centre_hz = 0.0;
        files::write_csv(&mut text, &files::Header { name: "hall", method: "balloon", rate: RATE, stamp: "now" }, &Band::Octave, &reports).unwrap();
        assert!(text.contains("hz,edt_s,t20_s,t30_s") && text.contains("1000.0,,1.234,") && text.contains("\nall,"), "{text}");
    }

    fn wav_header_for_test() -> [u8; 44] {
        files::wav_header(RATE, 100)
    }
}
