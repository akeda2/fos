//! Oversampling for nonlinear stages. A curve like `overdrive` makes harmonics of every partial; at the instrument's own rate
//! the ones above Nyquist fold back as tones that are not harmonic at all, and a tone-rich source (organ, strings) puts them
//! right in the audible band. Running the curve at four times the rate moves Nyquist to 2 x the rate, so the folded part is
//! removed by a low-pass before it is brought back down.
//!
//! Each 2x stage is a half-band FIR (Blackman-windowed sinc with cut-off at a quarter of the stage's rate): zero-stuff,
//! filter, apply the curve to both samples, filter, keep every second one. The first stage (rate -> 2 x rate) has the long
//! filter: it passes to about 18 kHz at 48 kHz and what it leaves between 24 and 30 kHz folds into the 18-24 kHz band
//! only. The second stage only has to keep the content above 2 x rate away, so a short filter will do.

use dsp::math;

use crate::overdrive;

/// A symmetric FIR of `N` (odd) taps over a circular history.
struct Fir<const N: usize> {
    taps: [f32; N],
    history: [f32; N],
    position: usize,
}

impl<const N: usize> Fir<N> {
    fn half_band() -> Self {
        let mut taps = [0.0; N];
        let middle = (N / 2) as i32;
        for (index, tap) in taps.iter_mut().enumerate() {
            let n = index as i32 - middle;
            let sinc = if n == 0 {
                0.5
            } else if n % 2 == 0 {
                0.0
            } else {
                let x = core::f32::consts::PI * n as f32 * 0.5;
                math::sin(x) / (2.0 * x)
            };
            let phase = 2.0 * core::f32::consts::PI * index as f32 / (N - 1) as f32;
            *tap = sinc * (0.42 - 0.5 * math::cos(phase) + 0.08 * math::cos(2.0 * phase));
        }
        Fir { taps, history: [0.0; N], position: 0 }
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.position = if self.position == 0 { N - 1 } else { self.position - 1 };
        self.history[self.position] = x;
    }

    #[inline]
    fn output(&self) -> f32 {
        let mut sum = 0.0;
        for (index, tap) in self.taps.iter().enumerate() {
            if *tap != 0.0 {
                sum += tap * self.history[(self.position + index) % N];
            }
        }
        sum
    }
}

/// One 2x stage: `process` takes a sample at the stage's rate and returns one, with `curve` run at twice that rate.
struct Stage<const N: usize> {
    up: Fir<N>,
    down: Fir<N>,
}

impl<const N: usize> Stage<N> {
    fn new() -> Self {
        Stage { up: Fir::half_band(), down: Fir::half_band() }
    }

    fn process(&mut self, x: f32, mut curve: impl FnMut(f32) -> f32) -> f32 {
        // Zero-stuffing halves the level, hence the 2.
        self.up.push(x);
        let first = curve(self.up.output() * 2.0);
        self.up.push(0.0);
        let second = curve(self.up.output() * 2.0);
        self.down.push(first);
        self.down.push(second);
        self.down.output()
    }
}

/// `fx::overdrive` run at 4x the sample rate. Amount 0 is a bypass (no filtering, no delay); the filters add a latency of a
/// few dozen samples while it is on.
pub struct Overdrive {
    outer: Stage<47>,
    inner: Stage<15>,
}

impl Overdrive {
    pub fn new() -> Self {
        Overdrive { outer: Stage::new(), inner: Stage::new() }
    }

    pub fn process(&mut self, sample: f32, amount: f32) -> f32 {
        if amount <= 0.0 {
            return sample;
        }
        let Overdrive { outer, inner } = self;
        outer.process(sample, |x| inner.process(x, |y| overdrive(y, amount)))
    }
}

impl Default for Overdrive {
    fn default() -> Self {
        Self::new()
    }
}
