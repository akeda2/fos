//! A small stereo reverb after Freeverb (Jezar): four parallel damped feedback combs and two series allpasses per channel, the
//! right channel's delays a few samples longer so the two ears hear different rooms. Delay lengths are the 44.1 kHz ones
//! scaled to the sample rate and capped at the buffers' size (above ~96 kHz the room just gets a little smaller).

const COMBS: usize = 4;
const ALLPASSES: usize = 2;
const COMB_LENGTH: [usize; COMBS] = [1116, 1188, 1277, 1356];
const ALLPASS_LENGTH: [usize; ALLPASSES] = [556, 441];
/// Extra delay of the right channel, in samples at 44.1 kHz.
const SPREAD: usize = 23;
const COMB_CAP: usize = 3072;
const ALLPASS_CAP: usize = 1280;
/// Feedback of the combs (the tail, about two seconds) and the high-frequency damping inside them.
const FEEDBACK: f32 = 0.84;
const DAMPING: f32 = 0.25;
const ALLPASS_FEEDBACK: f32 = 0.5;
/// The input is scaled down so that four combs at this feedback come out near the level that went in.
const INPUT_GAIN: f32 = 0.05;

struct Comb {
    buffer: [f32; COMB_CAP],
    length: usize,
    at: usize,
    filtered: f32,
}

struct Allpass {
    buffer: [f32; ALLPASS_CAP],
    length: usize,
    at: usize,
}

struct Side {
    combs: [Comb; COMBS],
    allpasses: [Allpass; ALLPASSES],
}

impl Side {
    const fn new() -> Self {
        const COMB: Comb = Comb { buffer: [0.0; COMB_CAP], length: 1, at: 0, filtered: 0.0 };
        const ALLPASS: Allpass = Allpass { buffer: [0.0; ALLPASS_CAP], length: 1, at: 0 };
        Side { combs: [COMB; COMBS], allpasses: [ALLPASS; ALLPASSES] }
    }

    fn set_rate(&mut self, rate: f32, extra: usize) {
        let scale = rate / 44100.0;
        for (comb, base) in self.combs.iter_mut().zip(COMB_LENGTH) {
            comb.length = (((base + extra) as f32 * scale) as usize).clamp(1, COMB_CAP);
        }
        for (allpass, base) in self.allpasses.iter_mut().zip(ALLPASS_LENGTH) {
            allpass.length = (((base + extra) as f32 * scale) as usize).clamp(1, ALLPASS_CAP);
        }
        self.clear();
    }

    fn clear(&mut self) {
        for comb in &mut self.combs {
            comb.buffer = [0.0; COMB_CAP];
            comb.at = 0;
            comb.filtered = 0.0;
        }
        for allpass in &mut self.allpasses {
            allpass.buffer = [0.0; ALLPASS_CAP];
            allpass.at = 0;
        }
    }

    fn process(&mut self, input: f32) -> f32 {
        let mut sum = 0.0;
        for comb in &mut self.combs {
            let out = comb.buffer[comb.at];
            comb.filtered = out * (1.0 - DAMPING) + comb.filtered * DAMPING;
            comb.buffer[comb.at] = input + comb.filtered * FEEDBACK;
            comb.at += 1;
            if comb.at >= comb.length {
                comb.at = 0;
            }
            sum += out;
        }
        for allpass in &mut self.allpasses {
            let stored = allpass.buffer[allpass.at];
            allpass.buffer[allpass.at] = sum + stored * ALLPASS_FEEDBACK;
            sum = stored - sum;
            allpass.at += 1;
            if allpass.at >= allpass.length {
                allpass.at = 0;
            }
        }
        sum
    }
}

pub struct Reverb {
    left: Side,
    right: Side,
    mix: f32,
}

impl Reverb {
    pub const fn new() -> Self {
        Reverb { left: Side::new(), right: Side::new(), mix: 0.0 }
    }

    pub fn set_sample_rate(&mut self, rate: f32) {
        self.left.set_rate(rate, 0);
        self.right.set_rate(rate, SPREAD);
    }

    /// Reverb level 0..=1 (0 is off and costs nothing). Switching it off empties the tail.
    pub fn set_mix(&mut self, mix: f32) {
        let mix = mix.clamp(0.0, 1.0);
        if mix == 0.0 && self.mix > 0.0 {
            self.left.clear();
            self.right.clear();
        }
        self.mix = mix;
    }

    pub fn mix(&self) -> f32 {
        self.mix
    }

    /// One stereo frame through the room: the dry sound (a little lower as the reverb comes up) plus the tail.
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.mix == 0.0 {
            return (left, right);
        }
        let input = (left + right) * INPUT_GAIN;
        let wet = self.mix * 3.0;
        let dry = 1.0 - 0.3 * self.mix;
        (left * dry + self.left.process(input) * wet, right * dry + self.right.process(input) * wet)
    }
}
