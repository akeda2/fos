//! Stereo to 5.1 by matrix, the way a passive surround decoder does it; no analysis of the music, so it is cheap and
//! predictable. Output order is SMPTE (and USB audio): L R C LFE Ls Rs.
//!
//! * **Centre**: the mid signal (L+R)/2, and half of it is taken out of the fronts, so a voice panned to the middle moves
//!   to the centre speaker while the energy stays about the same (`C` is scaled so that mono content keeps its power).
//!   With `center` off the C slot is silent and the fronts are untouched (many rooms have no centre speaker; the phantom
//!   image of the fronts does its job).
//! * **Surrounds**: the side signal (L-R), which holds the room and the wide-panned parts, low-passed at 7 kHz (rear
//!   speakers are dull in a real hall), delayed `delay` ms (the precedence effect keeps the sound in front) and sent in
//!   opposite polarity to Ls and Rs, with 1.3 ms more delay on Rs so the two are not identical. Below a few hundred Hz the
//!   two largely cancel when folded together, as with the matrix decoders this is modelled on.
//! * **LFE**: (L+R)/2 low-passed at `lfe` Hz with a Linkwitz-Riley 24 dB per octave filter. The fronts stay full range,
//!   so nothing is lost on a system whose sub is fed otherwise.
use dsp::math::sin_cos64;

/// The longest delay line: 40 ms plus the 1.3 ms offset at 384 kHz.
const DELAY_CAPACITY: usize = 16_384;
const SURROUND_CUTOFF_HZ: f64 = 7000.0;
const RS_EXTRA_MS: f32 = 1.3;
const CENTER_TAKEN_FROM_FRONTS: f32 = 0.5;
/// Keeps the power of mono content: 2 * (1 - 0.5)^2 + C^2 = 2.
const CENTER_GAIN: f32 = 1.2247449;
const SIDE_GAIN: f32 = core::f32::consts::FRAC_1_SQRT_2;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct UpmixSettings {
    /// Surround level, 0 (none) to 1 (full).
    pub width: f32,
    /// LFE crossover in Hz, 40 to 200.
    pub lfe_hz: f32,
    /// Surround delay in ms, 0 to 40.
    pub delay_ms: f32,
    /// Feed the centre channel.
    pub center: bool,
}

impl UpmixSettings {
    pub const DEFAULT: UpmixSettings = UpmixSettings { width: 0.7, lfe_hz: 100.0, delay_ms: 15.0, center: true };
}

#[derive(Clone, Copy)]
struct LowPass {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl LowPass {
    /// RBJ low-pass; Q 0.7071 is a Butterworth section.
    fn new(hz: f64, rate: f64, q: f64) -> LowPass {
        let (sin, cos) = sin_cos64(core::f64::consts::TAU * hz.min(0.45 * rate) / rate);
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = (1.0 - cos) / 2.0 / a0;
        LowPass { b: [b, 2.0 * b, b], a: [-2.0 * cos / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    #[inline]
    fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

pub struct Upmix {
    settings: UpmixSettings,
    side: LowPass,
    lfe: [LowPass; 2],
    delay: [f32; DELAY_CAPACITY],
    write: usize,
    ls_taps: usize,
    rs_taps: usize,
}

impl Upmix {
    const IDLE: LowPass = LowPass { b: [0.0; 3], a: [0.0; 2], z: [0.0; 2] };

    /// A silent upmix for a `static`; `configure` before use (it holds a 64 KiB delay line, so it is not built by value).
    pub const fn new() -> Upmix {
        Upmix { settings: UpmixSettings::DEFAULT, side: Self::IDLE, lfe: [Self::IDLE; 2], delay: [0.0; DELAY_CAPACITY], write: 0, ls_taps: 0, rs_taps: 0 }
    }

    /// Sets the stream's rate and the settings and clears all history.
    pub fn configure(&mut self, rate: u32, settings: UpmixSettings) {
        let rate = rate as f64;
        let taps = |ms: f32| ((ms as f64 * rate / 1000.0) as usize).min(DELAY_CAPACITY - 1);
        let q = core::f64::consts::FRAC_1_SQRT_2;
        let lfe = LowPass::new(settings.lfe_hz.clamp(40.0, 200.0) as f64, rate, q);
        let delay_ms = settings.delay_ms.clamp(0.0, 40.0);
        self.settings = settings;
        self.side = LowPass::new(SURROUND_CUTOFF_HZ, rate, q);
        self.lfe = [lfe, lfe];
        self.delay.fill(0.0);
        self.write = 0;
        self.ls_taps = taps(delay_ms);
        self.rs_taps = taps(delay_ms + RS_EXTRA_MS);
    }

    /// One stereo frame to L R C LFE Ls Rs.
    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 6] {
        let (left, right) = (frame[0], frame[1]);
        let mid = 0.5 * (left + right);
        let side = self.side.process((SIDE_GAIN * (left - right)) as f64) as f32;
        self.delay[self.write] = side;
        let tap = |taps: usize| self.delay[(self.write + DELAY_CAPACITY - taps) % DELAY_CAPACITY];
        let level = self.settings.width.clamp(0.0, 1.0);
        let (ls, rs) = (level * tap(self.ls_taps), -level * tap(self.rs_taps));
        self.write = (self.write + 1) % DELAY_CAPACITY;
        let first = self.lfe[0].process(mid as f64);
        let lfe = self.lfe[1].process(first) as f32;
        if self.settings.center {
            let taken = CENTER_TAKEN_FROM_FRONTS * mid;
            [left - taken, right - taken, CENTER_GAIN * mid, lfe, ls, rs]
        } else {
            [left, right, 0.0, lfe, ls, rs]
        }
    }

    /// Silences the filters and the delay line (after a seek).
    pub fn reset(&mut self) {
        self.side.z = [0.0; 2];
        for lfe in self.lfe.iter_mut() {
            lfe.z = [0.0; 2];
        }
        self.delay.fill(0.0);
    }
}

impl Default for Upmix {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(settings: UpmixSettings, left: impl Fn(usize) -> f32, right: impl Fn(usize) -> f32, frames: usize) -> Vec<[f32; 6]> {
        let mut up = Box::new(Upmix::new());
        up.configure(48000, settings);
        (0..frames).map(|n| up.process([left(n), right(n)])).collect()
    }

    fn rms(frames: &[[f32; 6]], channel: usize) -> f32 {
        let tail = &frames[frames.len() / 2..];
        (tail.iter().map(|f| f[channel] * f[channel]).sum::<f32>() / tail.len() as f32).sqrt()
    }

    fn tone(hz: f32) -> impl Fn(usize) -> f32 {
        move |n| (core::f32::consts::TAU * hz * n as f32 / 48000.0).sin()
    }

    #[test]
    fn mono_goes_to_the_centre_with_its_power_kept() {
        let frames = run(UpmixSettings::DEFAULT, tone(1000.0), tone(1000.0), 48000);
        let input = 0.5f32.sqrt();
        let power = 2.0 * rms(&frames, 0).powi(2) + rms(&frames, 2).powi(2);
        assert!((power - 2.0 * input * input).abs() < 0.02, "{power}");
        assert!(rms(&frames, 4) < 0.001 && rms(&frames, 5) < 0.001 && rms(&frames, 3) < 0.01);
        assert!(rms(&frames, 2) > rms(&frames, 0));
    }

    #[test]
    fn centre_off_leaves_the_fronts_alone_and_the_slot_silent() {
        let frames = run(UpmixSettings { center: false, ..UpmixSettings::DEFAULT }, tone(1000.0), tone(1000.0), 4800);
        assert!(frames.iter().all(|f| f[2] == 0.0));
        assert_eq!(frames[100][0], tone(1000.0)(100));
    }

    #[test]
    fn opposite_phase_goes_to_the_surrounds_delayed_and_opposite() {
        let frames = run(UpmixSettings { delay_ms: 10.0, ..UpmixSettings::DEFAULT }, tone(500.0), |n| -tone(500.0)(n), 48000);
        assert!(rms(&frames, 2) < 0.001, "no centre from a pure side signal");
        assert!(rms(&frames, 4) > 0.3 && (rms(&frames, 4) - rms(&frames, 5)).abs() < 0.02);
        // Before the delay has passed the surrounds are silent; after it Ls and Rs are in opposite phase.
        assert!(frames[..470].iter().all(|f| f[4] == 0.0));
        // The 1.3 ms offset is a few degrees at 50 Hz, so there the pair still nearly cancels (polarity is opposite).
        let low = run(UpmixSettings::DEFAULT, tone(50.0), |n| -tone(50.0)(n), 48000);
        let sum: Vec<[f32; 6]> = low.iter().map(|f| [0.0, 0.0, 0.0, 0.0, f[4] + f[5], 0.0]).collect();
        assert!(rms(&sum, 4) < 0.5 * rms(&low, 4));
    }

    #[test]
    fn lfe_takes_the_bass_only() {
        let low = run(UpmixSettings::DEFAULT, tone(50.0), tone(50.0), 48000);
        let high = run(UpmixSettings::DEFAULT, tone(1000.0), tone(1000.0), 48000);
        assert!(rms(&low, 3) > 0.6 * 0.707, "{}", rms(&low, 3));
        assert!(rms(&high, 3) < 0.002, "{}", rms(&high, 3));
    }

    #[test]
    fn surrounds_are_dull() {
        let hi = run(UpmixSettings::DEFAULT, tone(14000.0), |n| -tone(14000.0)(n), 48000);
        let lo = run(UpmixSettings::DEFAULT, tone(500.0), |n| -tone(500.0)(n), 48000);
        assert!(rms(&hi, 4) < 0.25 * rms(&lo, 4));
    }
}
