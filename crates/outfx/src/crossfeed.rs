//! Headphone cross-feed. On loudspeakers each ear hears both channels, the far one a little later and without its highs
//! (the head shades them); on headphones it hears one only, which puts hard-panned instruments "inside the head". The
//! cross-feed mixes the other channel back in the way the head would: low-passed, delayed and quieter (Bauer, Linkwitz,
//! Chu Moy and Meier all build this).
//!
//! `L' = L + g (D[LP(R)] - LP(L))`, likewise for `R'`, with `LP` a one-pole low-pass at `hz`, `D` a delay of `delay_us` and
//! `g` the feed level. Below the cutoff that is `(1-g) L + g R`: a hard-panned signal loses `g` of its level and the rest
//! goes to the other ear; above it the direct channel is untouched. A centred (mono) signal is exactly unchanged without
//! the delay; with it the two copies differ a little where the delay is a noticeable part of a period (an error of 6% of the
//! signal at 80 Hz at the defaults, growing with frequency), which is also what loudspeakers do. The two gains add up to 1,
//! so the output never exceeds the larger input. The delay is linearly interpolated,
//! which is exact enough below 1 kHz where the feed lives.

/// Longest delay the line holds, in samples (1 ms at 192 kHz).
const LINE: usize = 256;

/// The cross-feed's three knobs.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct CrossfeedSettings {
    /// How far below the direct channel the fed-in one is at low frequencies, 1..=20 dB: 4.5 is Linkwitz's, 6 Chu Moy's, a
    /// bigger number is gentler.
    pub level_db: f32,
    /// The low-pass corner on the fed-in channel (and the one taken from the direct), 200..=2000 Hz.
    pub hz: f32,
    /// The delay of the fed-in channel, 0..=1000 microseconds (the extra path around the head is about 250 us).
    pub delay_us: f32,
}

impl CrossfeedSettings {
    pub const DEFAULT: CrossfeedSettings = CrossfeedSettings { level_db: 6.0, hz: 700.0, delay_us: 250.0 };

    pub fn clamped(self) -> CrossfeedSettings {
        CrossfeedSettings { level_db: self.level_db.clamp(1.0, 20.0), hz: self.hz.clamp(200.0, 2000.0), delay_us: self.delay_us.clamp(0.0, 1000.0) }
    }
}

pub struct Crossfeed {
    rate: u32,
    /// Linear feed gain and the low-pass coefficient (`y += a (x - y)`).
    gain: f32,
    alpha: f32,
    delay: f32,
    low: [f32; 2],
    line: [[f32; LINE]; 2],
    at: usize,
}

impl Crossfeed {
    pub fn new(rate: u32, settings: CrossfeedSettings) -> Crossfeed {
        let mut feed = Crossfeed { rate, gain: 0.0, alpha: 0.0, delay: 0.0, low: [0.0; 2], line: [[0.0; LINE]; 2], at: 0 };
        feed.set(settings);
        feed
    }

    /// New settings without clearing the history (a slider moved while the music plays).
    pub fn set(&mut self, settings: CrossfeedSettings) {
        let settings = settings.clamped();
        let rate = self.rate as f32;
        self.gain = dsp::math::pow10(-settings.level_db / 20.0);
        self.alpha = 1.0 - dsp::math::exp64(-core::f64::consts::TAU * settings.hz as f64 / rate as f64) as f32;
        self.delay = (settings.delay_us * 1e-6 * rate).min((LINE - 2) as f32);
    }

    pub fn process(&mut self, frame: [f32; 2]) -> [f32; 2] {
        let whole = self.delay as usize;
        let fraction = self.delay - whole as f32;
        let mut low = [0.0f32; 2];
        let mut fed = [0.0f32; 2];
        for ear in 0..2 {
            self.low[ear] += self.alpha * (frame[ear] - self.low[ear]);
            low[ear] = self.low[ear];
            self.line[ear][self.at] = low[ear];
        }
        for ear in 0..2 {
            // The other channel's low-passed signal, `delay` samples ago.
            let other = ear ^ 1;
            let newer = self.line[other][(self.at + LINE - whole) % LINE];
            let older = self.line[other][(self.at + LINE - whole - 1) % LINE];
            fed[ear] = newer + (older - newer) * fraction;
        }
        self.at = (self.at + 1) % LINE;
        [frame[0] + self.gain * (fed[0] - low[0]), frame[1] + self.gain * (fed[1] - low[1])]
    }

    /// Clears the history (after a seek).
    pub fn reset(&mut self) {
        self.low = [0.0; 2];
        self.line = [[0.0; LINE]; 2];
        self.at = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48000;

    /// Gain of a sine at `hz` fed to the left channel only: (left out, right out) peak, after the filters settle.
    fn panned_left(settings: CrossfeedSettings, hz: f32) -> (f32, f32) {
        let mut feed = Crossfeed::new(RATE, settings);
        let (mut left, mut right) = (0.0f32, 0.0f32);
        for n in 0..RATE {
            let x = (core::f32::consts::TAU * hz * n as f32 / RATE as f32).sin();
            let y = feed.process([x, 0.0]);
            if n > RATE / 2 {
                left = left.max(y[0].abs());
                right = right.max(y[1].abs());
            }
        }
        (left, right)
    }

    #[test]
    fn a_hard_panned_bass_note_reaches_the_other_ear_a_set_level_down() {
        let (left, right) = panned_left(CrossfeedSettings { level_db: 6.0, hz: 700.0, delay_us: 250.0 }, 50.0);
        let g = 10f32.powf(-6.0 / 20.0);
        assert!((right - g).abs() < 0.03, "{right}");
        assert!((left - (1.0 - g)).abs() < 0.03, "{left}");
    }

    #[test]
    fn highs_stay_in_their_own_ear() {
        // An 8 kHz sine has six samples a period, so its sampled peak is sin 60 degrees = 0.87, not 1.
        let (left, right) = panned_left(CrossfeedSettings::DEFAULT, 8000.0);
        let peak = 0.866;
        assert!(right < 0.07 * peak, "{right}");
        assert!((left - peak).abs() < 0.05, "{left}");
    }

    #[test]
    fn a_centred_signal_is_unchanged_without_the_delay_and_nearly_with_it() {
        for (delay_us, hz, limit) in [(0.0, 440.0, 1e-5), (250.0, 80.0, 0.06)] {
            let mut feed = Crossfeed::new(RATE, CrossfeedSettings { delay_us, ..CrossfeedSettings::DEFAULT });
            let mut worst = 0.0f32;
            for n in 0..RATE {
                let x = (core::f32::consts::TAU * hz * n as f32 / RATE as f32).sin() * 0.8;
                let y = feed.process([x, x]);
                if n > 4800 {
                    worst = worst.max((y[0] - x).abs()).max((y[1] - x).abs());
                }
            }
            assert!(worst < limit, "{delay_us} us {hz} Hz: {worst}");
        }
    }

    #[test]
    fn the_fed_channel_is_delayed() {
        let mut feed = Crossfeed::new(RATE, CrossfeedSettings { level_db: 1.0, hz: 2000.0, delay_us: 500.0 });
        let mut first = None;
        for n in 0..200 {
            let y = feed.process([if n == 0 { 1.0 } else { 0.0 }, 0.0]);
            if first.is_none() && y[1] > 0.01 {
                first = Some(n);
            }
        }
        // 500 us is 24 samples; the low-pass lifts the output gradually from there.
        assert!(matches!(first, Some(24..=28)), "{first:?}");
    }

    #[test]
    fn settings_are_clamped_and_cannot_panic() {
        let mut feed = Crossfeed::new(192_000, CrossfeedSettings { level_db: -5.0, hz: 1.0, delay_us: 1e9 });
        for n in 0..1000 {
            let y = feed.process([(n % 7) as f32 / 7.0, 0.5]);
            assert!(y[0].is_finite() && y[1].is_finite());
        }
    }
}
