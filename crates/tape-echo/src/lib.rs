//! A tape echo in the manner of the Roland RE-201 Space Echo, as a `no_std`, allocation-free, hard-float library. An
//! instrument tape can use the whole of it or only the part it needs: `TapeEcho::process` takes one stereo frame and returns
//! one, with a handful of setters (all 0..=1 unless a name says otherwise), and `TapTempo` turns taps on a button into a delay.
//!
//! What it models. A loop of tape runs past a record head and three playback heads, spaced 1 : 2 : 3. The signal is recorded,
//! and each playback head hears it a little later. *Repeat rate* is the tape speed: it moves all three heads' delays together,
//! and because the speed is smoothed (a motor has inertia) turning it glides the pitch of the echoes up or down, as on the real
//! unit. *Intensity* feeds the heads' sum back to the record head; past about 0.9 the loop sustains and then runs away into
//! self-oscillation, which the tape's saturation keeps bounded. The *heads* selector chooses which of the three are heard and
//! fed back (the RE-201's twelve positions are these combinations with and without its reverb; here the heads are a bit mask and
//! the reverb is its own level). Tape character: every trip through the loop passes a high-frequency loss and a rumble filter,
//! so repeats get darker and thinner, and a soft saturation at the record head; wow and flutter wobble the read positions.
//! *Freeze* closes the loop: the input is shut, the feedback is 1 and the losses are bypassed, so what is on the tape keeps
//! going round. The reverb is a spring (`fx::Spring`) fed from the dry input, so reverb-only works with no heads selected.

#![cfg_attr(not(test), no_std)]

use dsp::math;
use fx::{Sine, Spring, phase_step};

mod tap;

pub use tap::TapTempo;

pub const HEAD1: u8 = 1;
pub const HEAD2: u8 = 2;
pub const HEAD3: u8 = 4;
/// The heads' delays are this multiple of head 1's.
const RATIO: [f32; 3] = [1.0, 2.0, 3.0];
/// Tape length in samples: 1.36 s at 48 kHz, so head 3 can sit 1.05 s behind the record head.
const CAP: usize = 65536;
/// Head 1's delay at the slowest and fastest tape speed, seconds (the limit also holds head 3 inside the tape at high rates).
const SLOWEST: f32 = 0.35;
const FASTEST: f32 = 0.05;
/// How the tape speed settles, seconds.
const MOTOR: f32 = 0.12;
const FREEZE_TIME: f32 = 0.03;
/// The most head 1's delay changes per sample: the echo's pitch moves by at most this fraction while the speed changes.
const MAX_SLEW: f32 = 0.25;

pub struct TapeEcho {
    rate: f32,
    tape: [f32; CAP],
    write: usize,
    sine: Sine,
    wow_phase: u32,
    flutter_phase: u32,
    wow_step: u32,
    flutter_step: u32,
    spring: Spring,

    repeat: f32,
    intensity: f32,
    heads: u8,
    echo_level: f32,
    dry: f32,
    input: f32,
    bass: f32,
    treble: f32,
    wow: f32,
    saturation: f32,
    spread: f32,
    frozen: bool,

    /// Head 1's delay now and the one the knob asks for, samples.
    delay_now: f32,
    delay_target: f32,
    /// Head 1's delay the repeat knob asks for before the speed bend, samples.
    knob_delay: f32,
    bend: f32,
    motor_coef: f32,
    freeze_coef: f32,
    /// 0 when running, 1 when frozen, moving between them over `FREEZE_TIME`.
    freeze: f32,
    /// Loop losses: a one-pole low-pass (the tape's high-frequency loss) and a one-pole rumble blocker.
    loss_coef: f32,
    loss_state: f32,
    rumble_coef: f32,
    rumble_state: f32,
    // Output tone: low and high shelves made from one-pole low-pass filters.
    bass_coef: f32,
    treble_coef: f32,
    bass_state: f32,
    treble_state: f32,
    bass_gain: f32,
    treble_gain: f32,
}

fn coef(hz: f32, rate: f32) -> f32 {
    1.0 - math::exp64(-2.0 * core::f64::consts::PI * hz as f64 / rate as f64) as f32
}

impl TapeEcho {
    pub fn new(sample_rate: u32) -> Self {
        let mut echo = TapeEcho {
            rate: 48000.0,
            tape: [0.0; CAP],
            write: 0,
            sine: Sine::new(),
            wow_phase: 0,
            flutter_phase: 0,
            wow_step: 0,
            flutter_step: 0,
            spring: Spring::new(sample_rate),
            repeat: 0.5,
            intensity: 0.4,
            heads: HEAD1 | HEAD2,
            echo_level: 0.7,
            dry: 1.0,
            input: 0.8,
            bass: 0.5,
            treble: 0.5,
            wow: 0.3,
            saturation: 0.4,
            spread: 0.5,
            frozen: false,
            delay_now: 0.0,
            delay_target: 0.0,
            knob_delay: 0.0,
            bend: 0.0,
            motor_coef: 0.0,
            freeze_coef: 0.0,
            freeze: 0.0,
            loss_coef: 0.0,
            loss_state: 0.0,
            rumble_coef: 0.0,
            rumble_state: 0.0,
            bass_coef: 0.0,
            treble_coef: 0.0,
            bass_state: 0.0,
            treble_state: 0.0,
            bass_gain: 1.0,
            treble_gain: 1.0,
        };
        echo.set_sample_rate(sample_rate);
        echo
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        self.rate = sample_rate.clamp(8000, 192000) as f32;
        self.tape = [0.0; CAP];
        self.write = 0;
        self.wow_step = phase_step(0.8, self.rate);
        self.flutter_step = phase_step(6.5, self.rate);
        self.motor_coef = 1.0 - math::exp64(-1.0 / (MOTOR as f64 * self.rate as f64)) as f32;
        self.freeze_coef = 1.0 - math::exp64(-1.0 / (FREEZE_TIME as f64 * self.rate as f64)) as f32;
        self.loss_coef = coef(5500.0, self.rate);
        self.rumble_coef = coef(90.0, self.rate);
        self.bass_coef = coef(200.0, self.rate);
        self.treble_coef = coef(3000.0, self.rate);
        self.spring.set_sample_rate(sample_rate);
        self.set_repeat_rate(self.repeat);
        self.delay_now = self.delay_target;
        self.loss_state = 0.0;
        self.rumble_state = 0.0;
        self.bass_state = 0.0;
        self.treble_state = 0.0;
    }

    /// The slowest and fastest delay head 1 can have, seconds (head 3 is three times as far).
    pub fn delay_range(&self) -> (f32, f32) {
        // Head 3 plus the wow swing has to stay inside the tape.
        let longest = ((CAP as f32 - 64.0) / self.rate / RATIO[2]).min(SLOWEST);
        (FASTEST, longest.max(FASTEST))
    }

    /// Tape speed, 0..=1: 0 is the slowest (head 1 about 350 ms behind) and 1 the fastest (50 ms), on a log scale.
    pub fn set_repeat_rate(&mut self, rate: f32) {
        self.repeat = rate.clamp(0.0, 1.0);
        let (fast, slow) = self.delay_range();
        let seconds = slow * math::pow10(-math::log10(slow / fast) * self.repeat);
        self.knob_delay = seconds * self.rate;
        self.retarget();
    }

    /// Speed bend, -1..=1: the tape runs up to an octave faster (+1, shorter delay, higher pitch) or slower (-1), on top of
    /// the repeat knob. The motor glides to it, so a pitch-bend wheel bends the repeats like a hand on the reel.
    pub fn set_speed_bend(&mut self, bend: f32) {
        self.bend = bend.clamp(-1.0, 1.0);
        self.retarget();
    }

    pub fn speed_bend(&self) -> f32 {
        self.bend
    }

    fn retarget(&mut self) {
        let slowest = self.delay_range().1 * self.rate * 2.0;
        let longest = (CAP as f32 - 64.0) / RATIO[2];
        self.delay_target = (self.knob_delay * math::pow10(-0.30103 * self.bend)).clamp(FASTEST * self.rate * 0.5, longest.min(slowest));
    }

    pub fn repeat_rate(&self) -> f32 {
        self.repeat
    }

    /// Sets the tape speed from head 1's delay in seconds (clamped to `delay_range`).
    pub fn set_delay(&mut self, seconds: f32) {
        let (fast, slow) = self.delay_range();
        let seconds = seconds.clamp(fast, slow);
        self.set_repeat_rate(math::log10(slow / seconds) / math::log10(slow / fast));
    }

    /// Head 1's delay at the speed the knob asks for, seconds.
    pub fn delay(&self) -> f32 {
        self.knob_delay / self.rate
    }

    /// `seconds` halved or doubled until it fits `delay_range`: a tap at 100 bpm (600 ms) becomes 300 ms.
    pub fn fit_delay(&self, seconds: f32) -> f32 {
        let (fast, slow) = self.delay_range();
        let mut seconds = seconds.max(0.001);
        while seconds > slow {
            seconds *= 0.5;
        }
        while seconds < fast {
            seconds *= 2.0;
        }
        seconds.clamp(fast, slow)
    }

    /// How much of the heads' sound goes back to the record head: 0 is a single echo, about 0.9 and up sustains, 1 runs away.
    pub fn set_intensity(&mut self, intensity: f32) {
        self.intensity = intensity.clamp(0.0, 1.0);
    }

    pub fn intensity(&self) -> f32 {
        self.intensity
    }

    /// Which heads are heard and fed back: `HEAD1 | HEAD2 | HEAD3`. 0 mutes the echo (reverb only).
    pub fn set_heads(&mut self, mask: u8) {
        self.heads = mask & 7;
    }

    pub fn heads(&self) -> u8 {
        self.heads
    }

    pub fn set_echo_level(&mut self, level: f32) {
        self.echo_level = level.clamp(0.0, 1.0);
    }

    pub fn echo_level(&self) -> f32 {
        self.echo_level
    }

    /// The dry signal in the output, 0..=1 (an insert wants 1, an effects send 0).
    pub fn set_dry(&mut self, dry: f32) {
        self.dry = dry.clamp(0.0, 1.0);
    }

    pub fn dry(&self) -> f32 {
        self.dry
    }

    /// How hard the signal is recorded onto the tape, 0..=1 (0.8 is unity gain).
    pub fn set_input(&mut self, input: f32) {
        self.input = input.clamp(0.0, 1.0);
    }

    pub fn input(&self) -> f32 {
        self.input
    }

    /// The spring reverb's level (0 is off).
    pub fn set_reverb(&mut self, mix: f32) {
        self.spring.set_mix(mix);
    }

    pub fn reverb(&self) -> f32 {
        self.spring.mix()
    }

    pub fn set_reverb_decay(&mut self, decay: f32) {
        self.spring.set_decay(decay);
    }

    pub fn reverb_decay(&self) -> f32 {
        self.spring.decay()
    }

    /// Echo tone: 0.5 is flat, 0 cuts and 1 boosts by 12 dB (bass below ~200 Hz, treble above ~3 kHz).
    pub fn set_bass(&mut self, bass: f32) {
        self.bass = bass.clamp(0.0, 1.0);
        self.bass_gain = math::pow10((self.bass - 0.5) * 2.0 * 12.0 / 20.0);
    }

    pub fn set_treble(&mut self, treble: f32) {
        self.treble = treble.clamp(0.0, 1.0);
        self.treble_gain = math::pow10((self.treble - 0.5) * 2.0 * 12.0 / 20.0);
    }

    pub fn bass(&self) -> f32 {
        self.bass
    }

    pub fn treble(&self) -> f32 {
        self.treble
    }

    /// Wow and flutter, 0..=1: slow (0.8 Hz) and fast (6.5 Hz) speed wobble, up to about +-0.3% in pitch.
    pub fn set_wow(&mut self, wow: f32) {
        self.wow = wow.clamp(0.0, 1.0);
    }

    pub fn wow(&self) -> f32 {
        self.wow
    }

    /// Tape saturation, 0..=1: how soon the record head squashes the signal (and how much the loop is held down).
    pub fn set_saturation(&mut self, saturation: f32) {
        self.saturation = saturation.clamp(0.0, 1.0);
    }

    pub fn saturation(&self) -> f32 {
        self.saturation
    }

    /// How far apart the heads sit in the stereo field, 0..=1 (0 is mono).
    pub fn set_spread(&mut self, spread: f32) {
        self.spread = spread.clamp(0.0, 1.0);
    }

    pub fn spread(&self) -> f32 {
        self.spread
    }

    /// Closes the loop: the input is shut and the echo goes round for as long as it is held.
    pub fn set_freeze(&mut self, frozen: bool) {
        self.frozen = frozen;
    }

    pub fn frozen(&self) -> bool {
        self.frozen
    }

    /// The sample `delay` samples behind the record head, linearly interpolated.
    #[inline]
    fn read(&self, delay: f32) -> f32 {
        let whole = delay as usize;
        let fraction = delay - whole as f32;
        let newer = self.tape[(self.write + CAP - whole) % CAP];
        let older = self.tape[(self.write + CAP - whole - 1) % CAP];
        newer + (older - newer) * fraction
    }

    /// One stereo frame in (summed to mono onto the tape), one out.
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        let dry_in = 0.5 * (left + right);
        let target = if self.frozen { 1.0 } else { 0.0 };
        self.freeze += self.freeze_coef * (target - self.freeze);
        let run = 1.0 - self.freeze;
        // The motor takes its time, and never runs the heads past a quarter of the tape speed (no rewinding).
        self.delay_now += (self.motor_coef * (self.delay_target - self.delay_now)).clamp(-MAX_SLEW, MAX_SLEW);

        // The wobble moves the read positions, which is what bends the pitch.
        self.wow_phase = self.wow_phase.wrapping_add(self.wow_step);
        self.flutter_phase = self.flutter_phase.wrapping_add(self.flutter_step);
        let swing = self.wow * run * 0.0004 * self.rate;
        let wobble = swing * (self.sine.at(self.wow_phase) + 0.25 * self.sine.at(self.flutter_phase));

        let mut heard = [0.0f32; 3];
        let mut sum = 0.0;
        let mut count = 0.0;
        for head in 0..3 {
            if self.heads & (1 << head) != 0 {
                let delay = (self.delay_now * RATIO[head] + wobble * math::sqrt(RATIO[head])).clamp(2.0, (CAP - 4) as f32);
                heard[head] = self.read(delay);
                sum += heard[head];
                count += 1.0;
            }
        }

        // Each head feeds back its share, so the loop gain is the intensity however many heads are chosen.
        let feedback_gain = self.freeze + run * 1.08 * self.intensity;
        let fed_back = if count > 0.0 { sum / count * feedback_gain } else { 0.0 };
        let recorded = dry_in * self.input * 1.25 * run + fed_back;

        // The tape: a rumble blocker and high-frequency loss on every trip (bypassed while frozen), then saturation.
        self.rumble_state += self.rumble_coef * run * (recorded - self.rumble_state);
        let mut signal = recorded - self.rumble_state;
        let loss = self.loss_coef + (1.0 - self.loss_coef) * self.freeze;
        self.loss_state += loss * (signal - self.loss_state);
        signal = self.loss_state;
        let squash = run * (0.3 + 2.7 * self.saturation);
        self.tape[self.write] = signal / (1.0 + squash * signal.abs());
        self.write = (self.write + 1) % CAP;

        // The echo: the heads' sum, panned, then the tone controls.
        let width = 0.35 * self.spread;
        let pan = [0.5 - width, 0.5, 0.5 + width];
        let (mut echo_l, mut echo_r) = (0.0, 0.0);
        for head in 0..3 {
            echo_l += heard[head] * (1.0 - pan[head]) * 2.0;
            echo_r += heard[head] * pan[head] * 2.0;
        }
        let (echo_l, echo_r) = (self.tone_left(echo_l), self.tone_right(echo_r));

        let (spring_l, spring_r) = self.spring.process(dry_in, dry_in);
        let (wet_l, wet_r) = (spring_l - dry_in, spring_r - dry_in);
        (left * self.dry + echo_l * self.echo_level + wet_l, right * self.dry + echo_r * self.echo_level + wet_r)
    }

    /// Fills `frames` in place with the echo of what is in them.
    pub fn process_block(&mut self, frames: &mut [[f32; 2]]) {
        for frame in frames.iter_mut() {
            let (l, r) = self.process(frame[0], frame[1]);
            *frame = [l, r];
        }
    }

    // The tone shelves share their low-pass states between the channels: the echo's two channels are nearly the same
    // signal, and one state keeps the filter cheap; the left runs first and the right reads the state it left.
    fn tone_left(&mut self, x: f32) -> f32 {
        self.bass_state += self.bass_coef * (x - self.bass_state);
        self.treble_state += self.treble_coef * (x - self.treble_state);
        x + (self.bass_gain - 1.0) * self.bass_state + (self.treble_gain - 1.0) * (x - self.treble_state)
    }

    fn tone_right(&mut self, x: f32) -> f32 {
        x + (self.bass_gain - 1.0) * self.bass_state + (self.treble_gain - 1.0) * (x - self.treble_state)
    }
}

#[cfg(test)]
mod tests;
