//! Logic steering for four decoded channels (`LF RF LB RB`): the passive SQ and QS matrices leave the neighbours of a
//! sound 3 dB below it (and a front-centre sound about 8 dB into the rears), and a logic decoder takes that away by
//! riding the gains of the weaker channels down while a channel clearly dominates.
//!
//! The rule is a downward expander relative to the strongest channel. Each channel's power is followed with a short
//! one-pole average; with `r` = power of the channel over the strongest one's, the channel's gain is `r^(slope/2)`
//! (amplitude), never below `floor`. A sound in one corner leaves its two neighbours at r = 0.5 and the far corner
//! at 0, so they are pushed down; a front-centre sound leaves the rears far below the fronts, so they go quiet; diffuse
//! music, with all four channels at similar levels, passes with gains near 1. Gains fall at the preset's attack and
//! recover at its (slower) release, so steering does not pump on every note, and below -70 dBFS (a silent groove) nothing
//! is steered at all.
//!
//! Presets: `Music` is gentle (slope 1, at most 6 dB, slow), what a stereo-compatible music disc wants; `Film` is
//! decisive (slope 3, up to 24 dB, quick). Like every matrix system, a steered decoder trades some of its ambience for
//! separation; with `Music` the fronts and rears stay balanced for pipe organ and orchestra.
use dsp::math::sqrt;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Preset {
    Music,
    Film,
}

impl Preset {
    pub fn name(self) -> &'static str {
        match self {
            Preset::Music => "music",
            Preset::Film => "film",
        }
    }

    pub fn parse(text: &str) -> Option<Preset> {
        match text {
            "music" => Some(Preset::Music),
            "film" => Some(Preset::Film),
            _ => None,
        }
    }

    pub fn next(self) -> Preset {
        match self {
            Preset::Music => Preset::Film,
            Preset::Film => Preset::Music,
        }
    }

    /// (slope, floor gain, power follower ms, attack ms, release ms)
    fn params(self) -> (u32, f32, f32, f32, f32) {
        match self {
            Preset::Music => (1, 0.5, 20.0, 10.0, 250.0),
            Preset::Film => (3, 0.063, 10.0, 5.0, 120.0),
        }
    }
}

/// Below this total power (-70 dBFS per channel) steering is off.
const GATE: f32 = 1e-7;

#[derive(Clone, Copy)]
pub struct Logic {
    preset: Preset,
    slope: u32,
    floor: f32,
    follow: f32,
    attack: f32,
    release: f32,
    power: [f32; 4],
    gain: [f32; 4],
}

impl Logic {
    pub const fn new() -> Self {
        Self { preset: Preset::Music, slope: 1, floor: 0.5, follow: 0.0, attack: 0.0, release: 0.0, power: [0.0; 4], gain: [1.0; 4] }
    }

    /// Sets the preset and the stream's rate; keeps the running state, so a preset can change while music plays.
    pub fn tune(&mut self, rate: u32, preset: Preset) {
        let (slope, floor, follow_ms, attack_ms, release_ms) = preset.params();
        let coefficient = |ms: f32| 1.0 - dsp::math::exp64(-1000.0 / (ms as f64 * rate as f64)) as f32;
        self.preset = preset;
        self.slope = slope;
        self.floor = floor;
        self.follow = coefficient(follow_ms);
        self.attack = coefficient(attack_ms);
        self.release = coefficient(release_ms);
    }

    pub fn preset(&self) -> Preset {
        self.preset
    }

    pub fn reset(&mut self) {
        self.power = [0.0; 4];
        self.gain = [1.0; 4];
    }

    /// The gains the last frame used (for a display).
    pub fn gains(&self) -> [f32; 4] {
        self.gain
    }

    /// `[LF, RF, LB, RB]` in and out.
    pub fn process(&mut self, frame: [f32; 4]) -> [f32; 4] {
        let mut strongest = 0.0f32;
        for (power, &x) in self.power.iter_mut().zip(&frame) {
            *power += self.follow * (x * x - *power);
            strongest = strongest.max(*power);
        }
        let mut out = [0.0; 4];
        for channel in 0..4 {
            let target = if strongest < GATE {
                1.0
            } else {
                let r = self.power[channel] / strongest;
                let g = match self.slope {
                    1 => sqrt(r),
                    2 => r,
                    _ => r * sqrt(r),
                };
                g.max(self.floor)
            };
            let rate = if target < self.gain[channel] { self.attack } else { self.release };
            self.gain[channel] += rate * (target - self.gain[channel]);
            out[channel] = frame[channel] * self.gain[channel];
        }
        out
    }
}

impl Default for Logic {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{QsDecoder, SqDecoder};

    const RATE: u32 = 48_000;

    fn tones(n: usize, offset: f32) -> Vec<f32> {
        (0..n)
            .map(|i| [220.0f32, 530.0, 1370.0, 3100.0, 6700.0].iter().map(|f| (core::f32::consts::TAU * (f + offset) * i as f32 / RATE as f32).sin()).sum::<f32>() * 0.2)
            .collect()
    }

    fn power_db(x: &[f32]) -> f32 {
        10.0 * (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32 + 1e-20).log10()
    }

    /// Four decoded channels through the logic, the levels (dB) of the last second.
    fn steered(decoded: &[[f32; 4]], preset: Preset) -> [f32; 4] {
        let mut logic = Logic::new();
        logic.tune(RATE, preset);
        let out: Vec<[f32; 4]> = decoded.iter().map(|&f| logic.process(f)).collect();
        let tail = &out[out.len() - 48_000..];
        [0, 1, 2, 3].map(|c| power_db(&tail.iter().map(|f| f[c]).collect::<Vec<_>>()))
    }

    fn qs_corner_left_front(n: usize) -> Vec<[f32; 4]> {
        // QS encode of LF is (0.924, 0.383) in phase.
        let mut decoder = QsDecoder::new();
        tones(n, 0.0).iter().map(|&x| decoder.process([0.923_879_5 * x, 0.382_683_4 * x])).collect()
    }

    #[test]
    fn a_corner_pushes_its_neighbours_down_more_with_film_than_music() {
        let decoded = qs_corner_left_front(3 * 48_000);
        let tail = &decoded[decoded.len() - 48_000..];
        let plain = [0, 1, 2, 3].map(|c| power_db(&tail.iter().map(|f| f[c]).collect::<Vec<_>>()));
        let music = steered(&decoded, Preset::Music);
        let film = steered(&decoded, Preset::Film);
        // neighbours RF and LB: -3 dB plain, about -6 with Music, well below -10 with Film
        assert!((plain[1] - plain[0] + 3.0).abs() < 0.5);
        assert!((music[1] - music[0] + 6.0).abs() < 1.5, "music {music:?}");
        assert!(film[1] - film[0] < -10.0 && film[2] - film[0] < -10.0, "film {film:?}");
        assert!((film[0] - plain[0]).abs() < 0.5, "the dominant channel keeps its level");
    }

    #[test]
    fn front_centre_empties_the_rears() {
        let mut decoder = QsDecoder::new();
        let decoded: Vec<[f32; 4]> = tones(3 * 48_000, 0.0).iter().map(|&x| decoder.process([x, x])).collect();
        let tail = &decoded[decoded.len() - 48_000..];
        let plain = [0, 1, 2, 3].map(|c| power_db(&tail.iter().map(|f| f[c]).collect::<Vec<_>>()));
        let steered = steered(&decoded, Preset::Music);
        assert!(steered[2] - steered[0] < plain[2] - plain[0] - 5.0, "{plain:?} -> {steered:?}");
    }

    #[test]
    fn sq_back_corner_keeps_the_back_and_quietens_the_fronts() {
        // SQ encode of LB: (-0.707 j, -0.707), through the same decoder pair the tape uses.
        let mut q = crate::Quadrature::new();
        let mut decoder = SqDecoder::new();
        let decoded: Vec<[f32; 4]> = tones(3 * 48_000, 0.0)
            .iter()
            .map(|&x| {
                let (same, j) = q.process(x);
                decoder.process([-0.707 * j, -0.707 * same])
            })
            .collect();
        let levels = steered(&decoded, Preset::Film);
        assert!(levels[2] > levels[0] + 8.0 && levels[2] > levels[1] + 8.0, "{levels:?}");
    }

    #[test]
    fn diffuse_sound_and_silence_pass_untouched() {
        let channels: Vec<Vec<f32>> = [0.0, 13.0, 29.0, 41.0].iter().map(|&o| tones(3 * 48_000, o)).collect();
        let decoded: Vec<[f32; 4]> = (0..channels[0].len()).map(|i| [channels[0][i], channels[1][i], channels[2][i], channels[3][i]]).collect();
        let plain = [0, 1, 2, 3].map(|c| power_db(&channels[c][2 * 48_000..]));
        let out = steered(&decoded, Preset::Film);
        for c in 0..4 {
            assert!((out[c] - plain[c]).abs() < 2.5, "channel {c}: {} vs {}", out[c], plain[c]);
        }
        let mut logic = Logic::new();
        logic.tune(RATE, Preset::Film);
        for _ in 0..1000 {
            assert_eq!(logic.process([0.0; 4]), [0.0; 4]);
        }
    }
}
