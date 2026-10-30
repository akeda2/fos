//! Stereo to 5.1 by matrix, the way a passive surround decoder does it; no analysis of the music, so it is cheap and
//! predictable. Output order is SMPTE (and USB audio): L R C LFE Ls Rs. A `Layout` leaves out what a smaller speaker
//! set lacks: 4.1 is L R LFE Ls Rs (no centre, fronts untouched), 4.0 is quad, L R Ls Rs, 2.1 is L R LFE.
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
//!   so nothing is lost on a system whose sub is fed otherwise. With `bass_mgmt` the fronts and the centre are high-passed
//!   at the same frequency with the matching Linkwitz-Riley filter, so the bass they lose is the bass the LFE carries (the
//!   two add up flat) and small speakers are not asked for it. The surrounds are left alone.
use dsp::math::sin_cos64;

/// The longest delay line: 40 ms plus the 1.3 ms offset at 384 kHz.
const DELAY_CAPACITY: usize = 16_384;
const SURROUND_CUTOFF_HZ: f64 = 7000.0;
const RS_EXTRA_MS: f32 = 1.3;
const CENTER_TAKEN_FROM_FRONTS: f32 = 0.5;
/// Keeps the power of mono content: 2 * (1 - 0.5)^2 + C^2 = 2.
const CENTER_GAIN: f32 = 1.2247449;
const SIDE_GAIN: f32 = core::f32::consts::FRAC_1_SQRT_2;

/// Which of the six upmix channels the device gets, in this order.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Layout {
    /// L R C LFE Ls Rs.
    Surround51,
    /// L R LFE Ls Rs: no centre speaker.
    Quad41,
    /// L R LFE: stereo with a sub.
    Stereo21,
    /// L R Ls Rs: quad, no centre and no sub.
    Quad40,
}

impl Layout {
    pub const ALL: [Layout; 4] = [Layout::Surround51, Layout::Quad41, Layout::Quad40, Layout::Stereo21];

    pub fn channels(self) -> usize {
        match self {
            Layout::Surround51 => 6,
            Layout::Quad41 => 5,
            Layout::Quad40 => 4,
            Layout::Stereo21 => 3,
        }
    }

    pub fn has_center(self) -> bool {
        self == Layout::Surround51
    }

    /// There is an LFE channel to carry what bass management takes out of the fronts.
    pub fn has_lfe(self) -> bool {
        matches!(self, Layout::Surround51 | Layout::Quad41 | Layout::Stereo21)
    }

    /// There are surround channels (so width and delay mean something).
    pub fn has_surround(self) -> bool {
        self != Layout::Stereo21
    }

    pub fn name(self) -> &'static str {
        match self {
            Layout::Surround51 => "5.1",
            Layout::Quad41 => "4.1",
            Layout::Quad40 => "4.0",
            Layout::Stereo21 => "2.1",
        }
    }

    pub fn parse(text: &str) -> Option<Layout> {
        Layout::ALL.into_iter().find(|layout| layout.name() == text)
    }

    /// The first of 5.1, 4.1, 4.0, 2.1 that fits `channels` device outputs, `None` below three.
    pub fn fitting(channels: usize) -> Option<Layout> {
        Layout::ALL.into_iter().find(|layout| layout.channels() <= channels)
    }

    /// Positions of the channels that carry full-range sound (not the centre, which is silent here, nor the LFE).
    pub fn full_range(self) -> &'static [usize] {
        match self {
            Layout::Surround51 => &[0, 1, 4, 5],
            Layout::Quad41 => &[0, 1, 3, 4],
            Layout::Quad40 => &[0, 1, 2, 3],
            Layout::Stereo21 => &[0, 1],
        }
    }

    /// Position of the LFE channel, if any.
    pub fn lfe_index(self) -> Option<usize> {
        match self {
            Layout::Surround51 => Some(3),
            Layout::Quad41 => Some(2),
            Layout::Stereo21 => Some(2),
            Layout::Quad40 => None,
        }
    }

    pub(crate) fn pick(self, six: [f32; 6]) -> [f32; 6] {
        match self {
            Layout::Surround51 => six,
            Layout::Quad41 => [six[0], six[1], six[3], six[4], six[5], 0.0],
            Layout::Quad40 => [six[0], six[1], six[4], six[5], 0.0, 0.0],
            Layout::Stereo21 => [six[0], six[1], six[3], 0.0, 0.0, 0.0],
        }
    }
}

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
    pub layout: Layout,
    /// High-pass the fronts and the centre at `lfe_hz` (they hand their bass to the LFE); needs a layout with an LFE.
    pub bass_mgmt: bool,
}

impl UpmixSettings {
    pub const DEFAULT: UpmixSettings = UpmixSettings { width: 0.7, lfe_hz: 100.0, delay_ms: 15.0, center: true, layout: Layout::Surround51, bass_mgmt: false };
}

#[derive(Clone, Copy)]
pub(crate) struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    /// RBJ low-pass; Q 0.7071 is a Butterworth section.
    pub(crate) fn new(hz: f64, rate: f64, q: f64) -> Biquad {
        let (sin, cos) = sin_cos64(core::f64::consts::TAU * hz.min(0.45 * rate) / rate);
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = (1.0 - cos) / 2.0 / a0;
        Biquad { b: [b, 2.0 * b, b], a: [-2.0 * cos / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    /// RBJ high-pass, same Q convention.
    fn high(hz: f64, rate: f64, q: f64) -> Biquad {
        let (sin, cos) = sin_cos64(core::f64::consts::TAU * hz.min(0.45 * rate) / rate);
        let alpha = sin / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b = (1.0 + cos) / 2.0 / a0;
        Biquad { b: [b, -2.0 * b, b], a: [-2.0 * cos / a0, (1.0 - alpha) / a0], z: [0.0; 2] }
    }

    #[inline]
    pub(crate) fn process(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

pub struct Upmix {
    settings: UpmixSettings,
    side: Biquad,
    lfe: [Biquad; 2],
    /// Two cascaded sections each for L, R and C when bass management is on.
    high: [[Biquad; 2]; 3],
    delay: [f32; DELAY_CAPACITY],
    write: usize,
    ls_taps: usize,
    rs_taps: usize,
}

impl Upmix {
    pub(crate) const IDLE: Biquad = Biquad { b: [0.0; 3], a: [0.0; 2], z: [0.0; 2] };

    /// A silent upmix for a `static`; `configure` before use (it holds a 64 KiB delay line, so it is not built by value).
    pub const fn new() -> Upmix {
        Upmix { settings: UpmixSettings::DEFAULT, side: Self::IDLE, lfe: [Self::IDLE; 2], high: [[Self::IDLE; 2]; 3], delay: [0.0; DELAY_CAPACITY], write: 0, ls_taps: 0, rs_taps: 0 }
    }

    /// Sets the stream's rate and the settings and clears all history.
    pub fn configure(&mut self, rate: u32, settings: UpmixSettings) {
        self.tune(rate, settings);
        self.delay.fill(0.0);
        self.write = 0;
    }

    /// Like `configure` but keeps the history, so a setting can be turned while the music plays (a touch slider).
    pub fn tune(&mut self, rate: u32, settings: UpmixSettings) {
        let rate = rate as f64;
        let taps = |ms: f32| ((ms as f64 * rate / 1000.0) as usize).min(DELAY_CAPACITY - 1);
        let q = core::f64::consts::FRAC_1_SQRT_2;
        let lfe = Biquad::new(settings.lfe_hz.clamp(40.0, 200.0) as f64, rate, q);
        let delay_ms = settings.delay_ms.clamp(0.0, 40.0);
        self.settings = settings;
        self.side = Biquad::new(SURROUND_CUTOFF_HZ, rate, q);
        self.lfe = [lfe, lfe];
        // Keep the state of the running filters; only the coefficients change.
        let high = Biquad::high(settings.lfe_hz.clamp(40.0, 200.0) as f64, rate, q);
        for section in self.high.iter_mut().flatten() {
            *section = Biquad { z: section.z, ..high };
        }
        self.ls_taps = taps(delay_ms);
        self.rs_taps = taps(delay_ms + RS_EXTRA_MS);
    }

    /// One stereo frame to the channels of the layout (the first `layout.channels()` of the result; 5.1 is L R C LFE Ls Rs).
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
        let mut six = if self.settings.center && self.settings.layout.has_center() {
            let taken = CENTER_TAKEN_FROM_FRONTS * mid;
            [left - taken, right - taken, CENTER_GAIN * mid, lfe, ls, rs]
        } else {
            [left, right, 0.0, lfe, ls, rs]
        };
        if self.settings.bass_mgmt && self.settings.layout.has_lfe() {
            for (channel, sections) in [0, 1, 2].into_iter().zip(self.high.iter_mut()) {
                let first = sections[0].process(six[channel] as f64);
                six[channel] = sections[1].process(first) as f32;
            }
        }
        self.settings.layout.pick(six)
    }

    /// Silences the filters and the delay line (after a seek).
    pub fn reset(&mut self) {
        self.side.z = [0.0; 2];
        for lfe in self.lfe.iter_mut() {
            lfe.z = [0.0; 2];
        }
        for section in self.high.iter_mut().flatten() {
            section.z = [0.0; 2];
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
    fn smaller_layouts_drop_centre_and_sub() {
        let side = |layout| run(UpmixSettings { layout, ..UpmixSettings::DEFAULT }, tone(500.0), |n| -tone(500.0)(n), 48000);
        let quad = side(Layout::Quad40);
        assert!(rms(&quad, 2) > 0.3 && rms(&quad, 3) > 0.3 && rms(&quad, 4) == 0.0);
        let mono = run(UpmixSettings { layout: Layout::Quad41, ..UpmixSettings::DEFAULT }, tone(60.0), tone(60.0), 48000);
        // No centre: the fronts keep the signal whole, and the sub is the third channel.
        assert!((rms(&mono, 0) - 0.707).abs() < 0.01 && rms(&mono, 2) > 0.3 && rms(&mono, 3) < 0.001);
        assert_eq!(Layout::fitting(5), Some(Layout::Quad41));
        assert_eq!(Layout::fitting(3), Some(Layout::Stereo21));
        assert_eq!(Layout::fitting(2), None);
    }

    #[test]
    fn stereo_with_a_sub_has_three_channels_and_bass_management_splits_at_the_crossover() {
        assert_eq!((Layout::Stereo21.channels(), Layout::fitting(3), Layout::parse("2.1")), (3, Some(Layout::Stereo21), Some(Layout::Stereo21)));
        let managed = UpmixSettings { layout: Layout::Stereo21, bass_mgmt: true, ..UpmixSettings::DEFAULT };
        let low = run(managed, tone(40.0), tone(40.0), 48000);
        let high = run(managed, tone(1000.0), tone(1000.0), 48000);
        // The fronts lose the bass to the sub; the treble stays in the fronts.
        assert!(rms(&low, 0) < 0.1 * 0.707 && rms(&low, 2) > 0.5 * 0.707, "{} {}", rms(&low, 0), rms(&low, 2));
        assert!((rms(&high, 0) - 0.707).abs() < 0.02 && rms(&high, 2) < 0.002);
        // The pair is an LR4 split: front plus sub, summed as signals, has the input's level at the crossover.
        let at = run(managed, tone(100.0), tone(100.0), 48000);
        let summed: Vec<[f32; 6]> = at.iter().map(|f| [f[0] + f[2], 0.0, 0.0, 0.0, 0.0, 0.0]).collect();
        assert!((rms(&summed, 0) - 0.707).abs() < 0.03, "{}", rms(&summed, 0));
        // Off, the fronts stay whole.
        let whole = run(UpmixSettings { bass_mgmt: false, ..managed }, tone(40.0), tone(40.0), 48000);
        assert!((rms(&whole, 0) - 0.707).abs() < 0.01);
        // No LFE in quad: bass management does nothing there.
        let quad = run(UpmixSettings { layout: Layout::Quad40, bass_mgmt: true, ..UpmixSettings::DEFAULT }, tone(40.0), tone(40.0), 48000);
        assert!((rms(&quad, 0) - 0.707).abs() < 0.01);
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
