//! A Rhodes-style electric piano as a `no_std`, allocation-free, hard-float library: MIDI messages and settings in, stereo f32
//! frames out. The tape owns the screen, touch and audio device.
//!
//! The sound is modal by default (`Model::Bar`; `Model` lists the other ways to make a tine, to be compared by ear). A Rhodes tine is a clamped bar, and an ideally clamped bar rings at 1 : 6.27 : 17.55 times its
//! fundamental (the roots of the cantilever beam equation). Each note is therefore a few decaying sinusoids, each kept as a
//! rotating phasor (`Mode`: one complex multiply a sample, no table, no oscillator state to click): the fundamental and a
//! weak second harmonic ring on, the 6.27x "bell" and the 17.55x "ting" die within a fraction of a second, and a hard hit
//! (velocity squared) puts far more into them, which is why a Rhodes bites when played hard. A short filtered noise burst is
//! the hammer. Every tine has its own pickup, a magnet and coil that sees the tine's displacement non-linearly and
//! asymmetrically and reads its change (`pickup`): that is the "bark", there only when a tine is struck hard and is close to
//! the pickup, rising and dying with its note. A vowel filter per voice follows (`Voicing::Vowel`): two formants that a hard
//! hit opens towards "a" and that close as the note dies, with a wah pedal on top; or, with `Voicing::Sweep`, a resonant
//! low-pass that sweeps down and the amplifier's mid hump on the sum. The sum gets the hammer's "b", then a tube-ish overdrive, a chorus, the suitcase's auto-pan tremolo, a
//! phaser, the tape echo of `crates/tape-echo` and the volume.
//!
//! Chain: each voice's tine -> pickup (bark) -> vowel or sweep; their sum + hammer -> (mid hump) -> limit -> drive -> chorus -> tremolo (pan) -> phaser -> tape echo -> volume. The mod wheel
//! (controller 1) raises the tremolo depth, the echo feedback and/or the bark, or works the wah, as `ModTarget` says; the
//! pitch wheel bends the echo's tape speed (a hand on the reel: the repeats slide, the played note does not); controller 11
//! is the wah pedal and 64 the sustain pedal.
#![cfg_attr(not(test), no_std)]

use dsp::math;
use fx::{Ensemble, Phaser, Sine, Svf, Overdrive, Wah, WahShape, overdrive, phase_step};
use tape_echo::TapeEcho;

/// The MIDI range of an 88-key piano.
pub const LOWEST: u8 = 21;
pub const HIGHEST: u8 = 108;
pub const VOICES: usize = 20;
/// Cantilever bar modes: the second and third resonance as multiples of the fundamental.
const BELL_RATIO: f32 = 6.267;
const TING_RATIO: f32 = 17.55;
/// Time for a released key's damper to take the note down by 60 dB.
const DAMPER_T60: f32 = 0.07;
/// The tonebar's slot in `Voice::modes`.
const TONEBAR: usize = 5;
/// The pickup's reach at full Bark: how many widths of the pickup's field a 127-velocity tine near middle C swings across
/// (the tine's displacement is then about 0.32).
const REACH: f32 = 2.0;
/// Where the tine rests relative to the middle of the pickup's field, in field widths: the asymmetry, the even harmonics.
const PICKUP_OFFSET: f32 = 0.4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModTarget {
    Tremolo,
    Echo,
    Bark,
    All,
    /// The wah: the mod wheel opens the vowel filter by hand, like controller 11 (not part of All).
    Wah,
}

impl ModTarget {
    pub const ALL: [ModTarget; 5] = [ModTarget::Tremolo, ModTarget::Echo, ModTarget::Bark, ModTarget::All, ModTarget::Wah];

    pub fn name(self) -> &'static str {
        ["Trem", "Echo fb", "Bark", "All", "Wah"][self as usize]
    }

    fn includes(self, other: ModTarget) -> bool {
        (self == ModTarget::All && other != ModTarget::Wah) || self == other
    }
}

/// How a struck tine is synthesised. They share the pickup, the amplifier and the effects; only the voice differs, so they
/// can be compared by ear on the same keyboard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Model {
    /// Cantilever-bar modes: a fundamental, a strong 6.27x bell and a 17.55x ting (the first version; rather a xylophone).
    Bar,
    /// A near-pure tine: the fundamental and a weak, very short bell, so the pickup supplies the harmonics.
    Tine,
    /// Two-operator FM in the manner of the DX7 "E.Piano": the tine's 14:1 click and a 1:1 index that decays into a sine.
    Fm,
    /// The `Tine` modes with a small FM voice on top for the bite of the attack.
    Hybrid,
    /// `Hybrid` with an inharmonic FM operator and a stronger bell: more metallic.
    Metal,
    /// Glockenspiel-like: a pure tine and its inharmonic overtones lifted into the kHz range, where they ring on.
    Chime,
}

impl Model {
    pub const ALL: [Model; 6] = [Model::Bar, Model::Tine, Model::Fm, Model::Hybrid, Model::Metal, Model::Chime];

    pub fn name(self) -> &'static str {
        ["Bar", "Tine", "FM", "Hybrid", "Metal", "Chime"][self as usize]
    }
}

/// One decaying sinusoid: `im` is the output, the pair rotates by the angle per sample and shrinks by the decay.
/// What follows each tine's pickup.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Voicing {
    /// Two formants that a hard hit opens towards "a" and that close as the bark dies (Vowel, Glide), sharpened by Q,
    /// as high as Range, and swept by hand with the wah pedal (controller 11 or the mod wheel's Wah target).
    Vowel,
    /// A resonant low-pass at a multiple of the pitch that sweeps down (the first try; brighter, "dunn" to "dinn").
    Sweep,
}

#[derive(Clone, Copy)]
struct Mode {
    re: f32,
    im: f32,
    cos: f32,
    sin: f32,
}

impl Mode {
    const SILENT: Mode = Mode { re: 0.0, im: 0.0, cos: 1.0, sin: 0.0 };

    /// Starts at zero output and rises smoothly (no click) with peak `amp`, at `hz`, 60 dB down after `t60` seconds.
    fn start(&mut self, amp: f32, hz: f32, t60: f32, rate: f32) {
        if hz >= 0.45 * rate || amp <= 0.0 {
            *self = Mode::SILENT;
            return;
        }
        let radius = math::pow10(-3.0 / (t60 * rate));
        let (sin, cos) = math::sin_cos64(2.0 * core::f64::consts::PI * hz as f64 / rate as f64);
        *self = Mode { re: amp, im: 0.0, cos: radius * cos as f32, sin: radius * sin as f32 };
    }

    #[inline]
    fn tick(&mut self) -> f32 {
        let out = self.im;
        let re = self.re * self.cos - self.im * self.sin;
        self.im = self.re * self.sin + self.im * self.cos;
        self.re = re;
        out
    }

    fn energy(&self) -> f32 {
        self.re * self.re + self.im * self.im
    }
}

#[derive(Clone, Copy)]
struct Voice {
    note: u8,
    /// Key down.
    held: bool,
    /// Kept ringing by the pedal after the key came up.
    sustained: bool,
    active: bool,
    /// Damper gain: 1 until the key is released (and the pedal is up), then falling.
    damper: f32,
    age: u32,
    /// The tine's modes; the last is the tonebar, which the pickup does not see (it sits beside the tine, not in front of
    /// the pole piece) and which is added after it.
    modes: [Mode; TONEBAR + 1],
    /// The FM voice: a carrier at the note's pitch phase-modulated by itself (`index_a`) and by a 14x operator (`index_b`),
    /// each with its own exponential envelope. `amp` 0 means the voice is not used.
    fm: Fm,
    /// This tine's own pickup (every tine has one).
    pickup: Pickup,
    /// A resonant low-pass after the pickup whose `g` (tan of the cutoff over the sample rate) starts high and relaxes to
    /// `g_end` by `g_k` a sample, with damping `k` (1/Q): a growl sweeping down through the harmonics, the "ow" of a
    /// Rhodes. A hard hit opens it further, rings it harder and sweeps it longer. Only when `swept`.
    swept: bool,
    sweep: Svf,
    g: f32,
    g_end: f32,
    g_k: f32,
    k: f32,
    /// The vowel filter (`Voicing::Vowel`, used instead of the sweep): a low-pass and two formant band-passes whose
    /// frequencies follow the openness, 0 a dark "u", 1 the top of the Range. `open` is what this hit adds to where the
    /// Vowel slider and the wah pedal put it; it relaxes to `open_end` by `open_k` a sample. `formant` is how far the two
    /// peaks stand out.
    vowel: bool,
    wah: Wah,
    open: f32,
    open_end: f32,
    open_k: f32,
    formant: f32,
}

impl Voice {
    /// The vowel filter (`fx::Wah`), one sample: this hit's lift relaxes towards its end, and the hit's own emphasis
    /// scales the peaks.
    fn speak(&mut self, x: f32, wah: &WahShape) -> f32 {
        let lift = self.open;
        self.open = self.open_end + (lift - self.open_end) * self.open_k;
        self.wah.tick(x, lift, self.formant, wah)
    }
}

fn tan_g(hz: f32, rate: f32) -> f32 {
    let (sin, cos) = math::sin_cos64(core::f64::consts::PI * hz as f64 / rate as f64);
    (sin / cos) as f32
}

#[derive(Clone, Copy)]
struct Fm {
    phase: u32,
    step: u32,
    phase_b: u32,
    step_b: u32,
    amp: f32,
    amp_k: f32,
    index_a: f32,
    index_a_k: f32,
    index_b: f32,
    index_b_k: f32,
}

impl Fm {
    const OFF: Fm = Fm { phase: 0, step: 0, phase_b: 0, step_b: 0, amp: 0.0, amp_k: 1.0, index_a: 0.0, index_a_k: 1.0, index_b: 0.0, index_b_k: 1.0 };

    #[inline]
    fn tick(&mut self, sine: &Sine) -> f32 {
        if self.amp <= 0.0 {
            return 0.0;
        }
        let turns = |radians: f32| ((radians * (1.0 / core::f32::consts::TAU)) as f64 * 4294967296.0) as i64 as u32;
        let own = sine.at(self.phase);
        let high = sine.at(self.phase_b);
        let out = self.amp * sine.at(self.phase.wrapping_add(turns(self.index_a * own + self.index_b * high)));
        self.phase = self.phase.wrapping_add(self.step);
        self.phase_b = self.phase_b.wrapping_add(self.step_b);
        self.amp *= self.amp_k;
        self.index_a *= self.index_a_k;
        self.index_b *= self.index_b_k;
        out
    }
}

impl Voice {
    const IDLE: Voice = Voice { note: 0, held: false, sustained: false, active: false, damper: 1.0, age: 0, modes: [Mode::SILENT; TONEBAR + 1], fm: Fm::OFF, pickup: Pickup::OFF, swept: false, sweep: Svf::ZERO, g: 1.0, g_end: 1.0, g_k: 1.0, k: 1.0, vowel: false, wah: Wah::ZERO, open: 0.0, open_end: 0.0, open_k: 1.0, formant: 0.0 };
}

pub struct EPiano {
    rate: f32,
    voices: [Voice; VOICES],
    pedal: bool,
    clock: u32,
    damper_coef: f32,
    model: Model,
    bell: f32,
    bark: f32,
    decay: f32,
    drive: f32,
    overdrive: Overdrive,
    chorus: f32,
    phaser: f32,
    tremolo_rate: f32,
    tremolo_depth: f32,
    echo_feedback: f32,
    volume: f32,
    mod_wheel: f32,
    mod_target: ModTarget,
    bend: f32,
    ensemble: Ensemble,
    phaser_fx: Phaser,
    echo: TapeEcho,
    sine: Sine,
    tremolo_phase: u32,
    tremolo_step: u32,
    noise: u32,
    thump: f32,
    thump_coef: f32,
    thump_state: f32,
    thump_pole: f32,
    /// The hammer's "b": a band-passed noise burst at the strike, louder the harder the hit and the closer the pickup.
    click: f32,
    click_coef: f32,
    click_filter: Svf,
    click_g: f32,
    /// The amplifier and speaker's mid hump (a band-pass added to the signal), deeper with Bark: the vowel of the bark.
    formant: Svf,
    formant_g: f32,
    touch: f32,
    /// The velocity floor (Soft), 0..=1, and the last velocity struck, for the screen.
    soft: f32,
    last_velocity: u8,
    voicing: Voicing,
    vowel: f32,
    glide: f32,
    /// The vowel filter's sharpness and how high it opens (Q, Range), and controller 11, the wah pedal, 0..=1.
    q: f32,
    range: f32,
    pedal_wah: f32,
    detune: f32,
    drive_state: f32,
    drive_pole: f32,
}

impl EPiano {
    /// A silent piano at `sample_rate` Hz with a gentle echo and a little chorus.
    pub fn new(sample_rate: u32) -> Self {
        let mut piano = EPiano {
            rate: 48000.0,
            voices: [Voice::IDLE; VOICES],
            pedal: false,
            clock: 0,
            damper_coef: 0.0,
            model: Model::Bar,
            bell: 0.45,
            bark: 0.5,
            decay: 0.5,
            drive: 0.0,
            overdrive: Overdrive::new(),
            chorus: 0.25,
            phaser: 0.0,
            tremolo_rate: 0.5,
            tremolo_depth: 0.0,
            echo_feedback: 0.3,
            volume: 0.7,
            mod_wheel: 0.0,
            mod_target: ModTarget::All,
            bend: 0.0,
            ensemble: Ensemble::new(sample_rate),
            phaser_fx: Phaser::new(sample_rate),
            echo: TapeEcho::new(sample_rate),
            sine: Sine::new(),
            tremolo_phase: 0,
            tremolo_step: 0,
            noise: 0x1234_5678,
            thump: 0.0,
            thump_coef: 0.0,
            thump_state: 0.0,
            thump_pole: 0.0,
            click: 0.0,
            click_coef: 0.0,
            click_filter: Svf::ZERO,
            click_g: 0.0,
            formant: Svf::ZERO,
            formant_g: 0.0,
            touch: 0.45,
            soft: 0.0,
            last_velocity: 0,
            voicing: Voicing::Vowel,
            vowel: 0.45,
            glide: 0.6,
            q: 0.4,
            range: 0.25,
            pedal_wah: 0.0,
            detune: 0.37,
            drive_state: 0.0,
            drive_pole: 0.0,
        };
        piano.echo.set_echo_level(0.3);
        piano.echo.set_reverb(0.0);
        piano.set_sample_rate(sample_rate);
        piano
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f32;
        self.rate = rate;
        self.ensemble.set_sample_rate(sample_rate);
        self.phaser_fx.set_sample_rate(sample_rate);
        self.echo.set_sample_rate(sample_rate);
        self.damper_coef = math::pow10(-3.0 / (DAMPER_T60 * rate));
        // The hammer thump lasts about 4 ms and is low-passed at 900 Hz so it is a thud, not a hiss.
        self.thump_coef = math::pow10(-3.0 / (0.012 * rate));
        self.thump_pole = one_pole(900.0, rate);
        self.click_coef = math::pow10(-3.0 / (0.015 * rate));
        self.click_g = tan_g(1800.0, rate);
        self.formant_g = tan_g(1000.0, rate);
        self.update_effects();
    }

    fn wheel_for(&self, target: ModTarget) -> f32 {
        if self.mod_target.includes(target) { self.mod_wheel } else { 0.0 }
    }

    fn update_effects(&mut self) {
        self.tremolo_step = phase_step(0.5 * math::pow10(1.3 * self.tremolo_rate), self.rate);
        self.ensemble.set_mix(self.chorus);
        self.ensemble.set_depth(0.7);
        self.phaser_fx.set_mix(self.phaser);
        self.phaser_fx.set_speed(0.35);
        self.phaser_fx.set_depth(0.8);
        self.phaser_fx.set_feedback(0.4);
        let feedback = self.echo_feedback;
        self.echo.set_intensity(feedback + (0.95 - feedback).max(0.0) * self.wheel_for(ModTarget::Echo));
        self.echo.set_speed_bend(self.bend);
        // The tube stage loses treble as it is driven harder.
        self.drive_pole = one_pole(14000.0 - 9000.0 * self.drive, self.rate);
    }

    /// Starts `note` (MIDI 21-108) at `velocity` 1-127; velocity 0 is a note off. A struck tine restarts.
    pub fn note_on(&mut self, note: u8, velocity: u8) {
        if velocity == 0 {
            return self.note_off(note);
        }
        if !(LOWEST..=HIGHEST).contains(&note) {
            return;
        }
        let index = self.pick_voice(note);
        self.clock = self.clock.wrapping_add(1);
        self.last_velocity = velocity;
        // Soft is a floor for keyboards that send high velocities for light playing: velocities up to it (0 to 100) are
        // the softest hit and the rest is stretched over the whole range. Touch then sets the curve: velocity to the power
        // 0.6 (light) to 2.2 (hard work to make it bark).
        let floor = 100.0 * self.soft;
        let played = ((velocity as f32 - floor) / (127.0 - floor)).max(1.0 / 127.0);
        let hits = math::pow10((0.6 + 1.6 * self.touch) * math::log10(played));
        let octaves = (note as f32 - 60.0) / 12.0;
        let hz = (440.0 * math::pow10_64((note as f64 - 69.0) / 12.0 * core::f64::consts::LOG10_2)) as f32;
        // Notes that decay faster the higher they are, and a decay slider from 0.8 s to 7.8 s (to -60 dB) at middle C.
        let t_fundamental = ((0.8 + 7.0 * self.decay) * math::pow10(-0.21 * octaves)).clamp(0.2, 20.0);
        let t_bell = (0.45 * math::pow10(-0.3 * octaves)).clamp(0.05, 0.6);
        // The bell rings in proportion to the hit squared and to the tone control; near the top it fades before it aliases.
        let fade = ((0.45 * self.rate - hz * BELL_RATIO) / (0.1 * self.rate)).clamp(0.0, 1.0);
        let strike = self.bell * hits * hits * fade;
        let rate = self.rate;
        // A tine's overtones sound as a chime only when they are high: low notes lift them by whole octaves (keeping the
        // inharmonic ratio) to at least `floor` Hz. A partial too near Nyquist fades out before it aliases.
        let lifted = |ratio: f32, floor: f32| {
            let mut ratio = ratio;
            while ratio * hz < floor && ratio * hz * 2.0 < 0.4 * rate {
                ratio *= 2.0;
            }
            ratio
        };
        let fade_at = |f: f32| ((0.45 * rate - f) / (0.1 * rate)).clamp(0.0, 1.0);
        let metal_ratio = lifted(5.43, 2500.0);
        let fm = match self.model {
            Model::Fm => self.fm_voice(hz, hits, 0.32, t_fundamental, 14.0, 0.15),
            Model::Hybrid => self.fm_voice(hz, hits, 0.10, t_fundamental, 14.0, 0.15),
            Model::Metal => self.fm_voice(hz, hits, 0.11, t_fundamental, metal_ratio, 0.30),
            _ => Fm::OFF,
        };
        // Brightness: the voice's resonant low-pass opens to 6-70 times the pitch (higher the harder the hit and the
        // brighter the tone), rings at a Q of 0.9-3.5 by the hit, and closes to 3-13 times the pitch in 0.25-1.15 s.
        let top = (hz * (6.0 + 64.0 * hits * (0.3 + 0.7 * self.bell))).min(0.4 * rate).min(16000.0);
        // Not `clamp`: a soft low note's `top` is below 600 Hz, and then the filter simply stays at `top`.
        let bottom = (hz * (3.0 + 10.0 * self.bell)).max(600.0).min(top);
        // The pickup reaches less on high notes: their tines swing less, and their harmonics would alias.
        let reach = REACH / 0.32 * self.bark / (0.85 + hz / 1500.0);
        let voice = &mut self.voices[index];
        *voice = Voice {
            note,
            held: true,
            sustained: false,
            active: true,
            damper: 1.0,
            age: self.clock,
            modes: [Mode::SILENT; TONEBAR + 1],
            fm,
            pickup: Pickup::new(hz, reach, rate),
            swept: false,
            sweep: Svf::ZERO,
            g: 1.0,
            g_end: 1.0,
            g_k: 1.0,
            k: 1.0,
            vowel: false,
            wah: Wah::ZERO,
            open: 0.0,
            open_end: 0.0,
            open_k: 1.0,
            formant: 0.0,
        };
        if self.model == Model::Bar || self.model == Model::Chime {
        } else if self.voicing == Voicing::Vowel {
            // The vowel: a hard hit opens it (towards "a", by Glide) from where Vowel sets it, and it closes again as the
            // bark dies, to a little below the Vowel setting: "baouw". A soft hit barely moves it: "plong".
            let (vowel, glide) = (self.vowel, self.glide);
            voice.vowel = true;
            voice.open = glide * hits * (1.2 - vowel);
            voice.open_end = -0.6 * glide * vowel;
            voice.open_k = math::pow10(-0.4342945 / ((0.06 + 0.3 * glide) * (0.5 + hits) * rate));
            voice.formant = 0.8 + 2.2 * hits;
        } else {
            voice.swept = true;
            voice.g = tan_g(top, rate);
            voice.g_end = tan_g(bottom, rate);
            voice.g_k = math::pow10(-0.4342945 / ((0.25 + 0.9 * hits) * rate));
            voice.k = 1.0 / (0.9 + 2.6 * hits * hits);
        }
        let beat = (0.05 + 4.0 * self.detune * self.detune) * math::sqrt(hz / 261.63);
        // The tonebar rings a hair off the tine's pitch, so a held note shimmers slowly.
        let tonebar = |voice: &mut Voice| voice.modes[TONEBAR].start(0.14 * hits, hz + beat, t_fundamental, rate);
        let thump = match self.model {
            Model::Bar => {
                voice.modes[0].start(0.32 * hits, hz, t_fundamental, rate);
                voice.modes[1].start(0.10 * hits * (0.5 + hits), 2.0 * hz, 0.5 * t_fundamental, rate);
                voice.modes[2].start(0.35 * strike, BELL_RATIO * hz, t_bell, rate);
                voice.modes[3].start(0.12 * strike, TING_RATIO * hz, 0.25 * t_bell, rate);
                0.5
            }
            Model::Tine | Model::Hybrid | Model::Metal => {
                // The tine rings almost purely; its second mode is a short, quiet chime. The pickup does the rest.
                voice.modes[0].start(0.32 * hits, hz, t_fundamental, rate);
                voice.modes[1].start(0.04 * hits, 2.0 * hz, 0.3 * t_fundamental, rate);
                let (chime, floor) = if self.model == Model::Metal { (0.22, 2500.0) } else { (0.10, 2000.0) };
                let f = hz * lifted(BELL_RATIO, floor);
                voice.modes[2].start(chime * self.bell * hits * hits * fade_at(f), f, 0.4 * t_bell.max(0.3), rate);
                tonebar(voice);
                0.04
            }
            Model::Fm => {
                tonebar(voice);
                0.03
            }
            Model::Chime => {
                voice.modes[0].start(0.32 * hits, hz, t_fundamental, rate);
                voice.modes[1].start(0.03 * hits, 2.0 * hz, 0.3 * t_fundamental, rate);
                let bright = self.bell * hits * hits;
                let (f2, f4) = (hz * lifted(BELL_RATIO, 2500.0), hz * lifted(TING_RATIO, 5000.0));
                voice.modes[2].start(0.12 * bright * fade_at(f2), f2, 0.4 + 0.8 * self.decay, rate);
                voice.modes[4].start(0.06 * bright * fade_at(f4), f4, 0.25 + 0.5 * self.decay, rate);
                tonebar(voice);
                0.02
            }
        };
        self.thump += thump * hits;
        self.click += 0.25 * hits * hits * hits * (0.2 + 0.8 * self.bark);
    }

    /// An FM voice at `hz`: amplitude `amp`, a 1:1 index that falls over about a second (a hard hit starts higher, so it
    /// is brighter) and the 14:1 index, the tine's click, which is gone in `t_click` seconds and scaled by the Bell control.
    fn fm_voice(&self, hz: f32, hits: f32, amp: f32, t_fundamental: f32, ratio: f32, t_click: f32) -> Fm {
        let rate = self.rate;
        let decay_k = |t60: f32| math::pow10(-3.0 / (t60 * rate));
        let fade = ((0.45 * rate - hz * ratio) / (0.1 * rate)).clamp(0.0, 1.0);
        Fm {
            phase: 0,
            step: phase_step(hz, rate),
            phase_b: 0,
            step_b: phase_step(hz * ratio, rate),
            amp: amp * hits,
            amp_k: decay_k(t_fundamental),
            index_a: (0.15 + 1.6 * hits) * (0.4 + self.bell),
            index_a_k: decay_k(0.9 * t_fundamental.min(2.0)),
            index_b: 1.4 * hits * hits * self.bell * fade,
            index_b_k: decay_k(t_click),
        }
    }

    /// The voice for a new `note`: the one already ringing it, a free one, else the most released (or oldest) one.
    fn pick_voice(&self, note: u8) -> usize {
        let key = |voice: &Voice| (voice.held as u32, (voice.damper * 1e6) as u32, voice.age);
        self.voices
            .iter()
            .position(|voice| voice.active && voice.note == note)
            .or_else(|| self.voices.iter().position(|voice| !voice.active))
            .unwrap_or_else(|| (0..VOICES).min_by_key(|&index| key(&self.voices[index])).unwrap_or(0))
    }

    pub fn note_off(&mut self, note: u8) {
        for voice in self.voices.iter_mut().filter(|voice| voice.active && voice.held && voice.note == note) {
            voice.held = false;
            voice.sustained = self.pedal;
        }
    }

    pub fn is_held(&self, note: u8) -> bool {
        self.voices.iter().any(|voice| voice.active && voice.held && voice.note == note)
    }

    pub fn held_count(&self) -> usize {
        self.voices.iter().filter(|voice| voice.active && voice.held).count()
    }

    pub fn all_notes_off(&mut self) {
        for voice in self.voices.iter_mut() {
            voice.held = false;
            voice.sustained = false;
        }
    }

    /// The sustain pedal: while down, released keys keep ringing; lifting it lets the dampers fall.
    pub fn set_sustain(&mut self, down: bool) {
        self.pedal = down;
        if !down {
            for voice in self.voices.iter_mut() {
                voice.sustained = false;
            }
        }
    }

    pub fn sustain(&self) -> bool {
        self.pedal
    }

    /// Handles one MIDI message: note on/off, pitch bend, controllers 1 (mod wheel), 11 (expression: the wah pedal), 64
    /// (sustain), 120 and 123 (all notes
    /// off). Returns whether it was one of those.
    pub fn midi(&mut self, message: &[u8]) -> bool {
        let [status, data1, data2] = match *message {
            [status, data1, data2] => [status, data1, data2],
            [status, data1] => [status, data1, 0],
            _ => return false,
        };
        match status & 0xf0 {
            0x90 => self.note_on(data1, data2),
            0x80 => self.note_off(data1),
            0xe0 => self.set_pitch_bend(((data2 as i32) << 7 | data1 as i32) as f32 / 8192.0 - 1.0),
            0xb0 => match data1 {
                1 => self.set_mod_wheel(data2 as f32 / 127.0),
                11 => self.set_wah_pedal(data2 as f32 / 127.0),
                64 => self.set_sustain(data2 >= 64),
                120 | 123 => self.all_notes_off(),
                _ => return false,
            },
            // A program change picks the model: programs 0-5 are Bar, Tine, FM, Hybrid, Metal and Chime.
            0xc0 => self.set_model(Model::ALL[data1 as usize % Model::ALL.len()]),
            _ => return false,
        }
        true
    }

    /// The tine synthesis for notes struck from now on; notes already ringing keep theirs.
    pub fn set_model(&mut self, model: Model) {
        self.model = model;
    }

    pub fn model(&self) -> Model {
        self.model
    }

    /// The pitch wheel, -1..=1: it bends the echo's tape speed by up to an octave.
    pub fn set_pitch_bend(&mut self, bend: f32) {
        // A wheel at rest reads a few counts off centre; do not let that detune the echo.
        self.bend = if bend.abs() < 0.02 { 0.0 } else { bend.clamp(-1.0, 1.0) };
        self.echo.set_speed_bend(self.bend);
    }

    pub fn pitch_bend(&self) -> f32 {
        self.bend
    }

    /// Tone bar (0) to bell (1): how much the tine's high resonances ring.
    pub fn set_bell(&mut self, bell: f32) {
        self.bell = bell.clamp(0.0, 1.0);
    }

    pub fn bell(&self) -> f32 {
        self.bell
    }

    /// How far the tonebar is detuned from the tine, 0..=1: the held note's shimmer, from 0.05 Hz to 4 Hz at middle C.
    pub fn set_detune(&mut self, detune: f32) {
        self.detune = detune.clamp(0.0, 1.0);
    }

    pub fn detune(&self) -> f32 {
        self.detune
    }

    /// How close the tines sit to their pickups, 0..=1: how much a hard hit barks.
    pub fn set_bark(&mut self, bark: f32) {
        self.bark = bark.clamp(0.0, 1.0);
    }

    pub fn bark(&self) -> f32 {
        self.bark
    }

    /// Touch, 0..=1: how hard a key must be struck for a loud, barking note (the velocity curve).
    pub fn set_touch(&mut self, touch: f32) {
        self.touch = touch.clamp(0.0, 1.0);
    }

    pub fn touch(&self) -> f32 {
        self.touch
    }

    /// Soft, 0..=1: the velocity floor, 0 to 100. Velocities up to it play the softest note, the rest is stretched to
    /// the full range: for a keyboard that sends high velocities for light playing (the screen shows the last one).
    pub fn set_soft(&mut self, soft: f32) {
        self.soft = soft.clamp(0.0, 1.0);
    }

    pub fn soft(&self) -> f32 {
        self.soft
    }

    /// The velocity of the last note struck (0 before any).
    pub fn last_velocity(&self) -> u8 {
        self.last_velocity
    }

    /// What follows each tine's pickup, for notes struck after the switch.
    pub fn set_voicing(&mut self, voicing: Voicing) {
        self.voicing = voicing;
    }

    pub fn voicing(&self) -> Voicing {
        self.voicing
    }

    /// Where the vowel rests, 0..=1: a dark "u" to an open "a" (`Voicing::Vowel`).
    pub fn set_vowel(&mut self, vowel: f32) {
        self.vowel = vowel.clamp(0.0, 1.0);
    }

    pub fn vowel(&self) -> f32 {
        self.vowel
    }

    /// How far a hard hit opens the vowel and how slowly it closes, 0..=1 (0: a fixed vowel).
    pub fn set_glide(&mut self, glide: f32) {
        self.glide = glide.clamp(0.0, 1.0);
    }

    pub fn glide(&self) -> f32 {
        self.glide
    }

    /// The formants' sharpness, 0..=1 (Q 1.5 to 12); the sharper, the less of the unfiltered sound is kept beside them.
    pub fn set_q(&mut self, q: f32) {
        self.q = q.clamp(0.0, 1.0);
    }

    pub fn q(&self) -> f32 {
        self.q
    }

    /// How high the vowel filter opens, 0..=1: the first formant at full openness, 600 Hz to 2.4 kHz (about 850 by
    /// default; 750 is an "a", above it a wah's squeal).
    pub fn set_range(&mut self, range: f32) {
        self.range = range.clamp(0.0, 1.0);
    }

    pub fn range(&self) -> f32 {
        self.range
    }

    /// The wah pedal (controller 11), 0..=1: opens the vowel filter of every ringing note from the Vowel setting to past
    /// the top of the Range. The mod wheel does the same with the Wah target; the further of the two counts.
    pub fn set_wah_pedal(&mut self, pedal: f32) {
        self.pedal_wah = pedal.clamp(0.0, 1.0);
    }

    pub fn wah_pedal(&self) -> f32 {
        self.pedal_wah
    }

    fn wah(&self) -> WahShape {
        WahShape::new(self.vowel, self.pedal_wah.max(self.wheel_for(ModTarget::Wah)), self.q, self.range, self.rate)
    }

    /// Decay time of newly struck notes, 0..=1.
    pub fn set_decay(&mut self, decay: f32) {
        self.decay = decay.clamp(0.0, 1.0);
    }

    pub fn decay(&self) -> f32 {
        self.decay
    }

    /// The tube stage, 0..=1 (0 is clean).
    pub fn set_drive(&mut self, drive: f32) {
        self.drive = drive.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn drive(&self) -> f32 {
        self.drive
    }

    pub fn set_chorus(&mut self, mix: f32) {
        self.chorus = mix.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn chorus(&self) -> f32 {
        self.chorus
    }

    pub fn set_phaser(&mut self, mix: f32) {
        self.phaser = mix.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn phaser(&self) -> f32 {
        self.phaser
    }

    /// Tremolo speed, 0..=1: 0.5 Hz to 10 Hz (exponential).
    pub fn set_tremolo_rate(&mut self, rate: f32) {
        self.tremolo_rate = rate.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn tremolo_rate(&self) -> f32 {
        self.tremolo_rate
    }

    pub fn set_tremolo_depth(&mut self, depth: f32) {
        self.tremolo_depth = depth.clamp(0.0, 1.0);
    }

    pub fn tremolo_depth(&self) -> f32 {
        self.tremolo_depth
    }

    pub fn set_echo_level(&mut self, level: f32) {
        self.echo.set_echo_level(level);
    }

    pub fn echo_level(&self) -> f32 {
        self.echo.echo_level()
    }

    pub fn set_echo_repeat(&mut self, rate: f32) {
        self.echo.set_repeat_rate(rate);
    }

    pub fn echo_repeat(&self) -> f32 {
        self.echo.repeat_rate()
    }

    /// The echo's feedback slider; the mod wheel adds to it while it steers the echo.
    pub fn set_echo_feedback(&mut self, feedback: f32) {
        self.echo_feedback = feedback.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn echo_feedback(&self) -> f32 {
        self.echo_feedback
    }

    /// The spring reverb of the echo unit, 0..=1.
    pub fn set_reverb(&mut self, mix: f32) {
        self.echo.set_reverb(mix);
    }

    pub fn reverb(&self) -> f32 {
        self.echo.reverb()
    }

    /// The echo unit for the rest of its knobs (heads, wow, tone, tap tempo through `set_delay`).
    pub fn echo_mut(&mut self) -> &mut TapeEcho {
        &mut self.echo
    }

    pub fn echo(&self) -> &TapeEcho {
        &self.echo
    }

    pub fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
    }

    pub fn volume(&self) -> f32 {
        self.volume
    }

    /// The mod wheel, 0..=1: more tremolo, more echo feedback and/or more bark, as `mod_target` says.
    pub fn set_mod_wheel(&mut self, wheel: f32) {
        self.mod_wheel = wheel.clamp(0.0, 1.0);
        self.update_effects();
    }

    pub fn mod_wheel(&self) -> f32 {
        self.mod_wheel
    }

    pub fn set_mod_target(&mut self, target: ModTarget) {
        self.mod_target = target;
        self.update_effects();
    }

    pub fn mod_target(&self) -> ModTarget {
        self.mod_target
    }

    /// The tube stage: a bias gives even harmonics, the squash is `fx::Overdrive` (4x oversampled), and the highs go as it is driven.
    fn tube(&mut self, x: f32) -> f32 {
        if self.drive <= 0.0 {
            return x;
        }
        let bias = 0.25 * self.drive;
        let y = self.overdrive.process(x + bias, self.drive) - overdrive(bias, self.drive);
        self.drive_state += self.drive_pole * (y - self.drive_state);
        self.drive_state
    }

    /// Fills `frames` with stereo output.
    pub fn render(&mut self, frames: &mut [[f32; 2]]) {
        let bark = self.bark + (1.0 - self.bark) * self.wheel_for(ModTarget::Bark);
        // The mod wheel's bark moves the tines closer to the pickups of the notes already ringing too.
        let bark_scale = if self.bark > 0.0 { bark / self.bark } else { 0.0 };
        let depth = self.tremolo_depth + (1.0 - self.tremolo_depth) * self.wheel_for(ModTarget::Tremolo);
        let wah = self.wah();
        for frame in frames.iter_mut() {
            let mut sum = 0.0;
            for voice in self.voices.iter_mut().filter(|voice| voice.active) {
                let mut tine = voice.fm.tick(&self.sine);
                for mode in voice.modes[..TONEBAR].iter_mut() {
                    tine += mode.tick();
                }
                let tonebar = voice.modes[TONEBAR].tick() * voice.damper;
                let mut sample = voice.pickup.tick(tine * voice.damper, bark_scale) + tonebar;
                if voice.swept {
                    sample = voice.sweep.tick(sample, voice.g, voice.k).1;
                    voice.g = voice.g_end + (voice.g - voice.g_end) * voice.g_k;
                } else if voice.vowel {
                    sample = voice.speak(sample, &wah);
                }
                sum += sample;
                if !voice.held && !voice.sustained {
                    voice.damper *= self.damper_coef;
                }
                if voice.damper < 1e-4 || (voice.modes[0].energy() < 1e-12 && voice.fm.amp * voice.fm.amp < 1e-12) {
                    voice.active = false;
                }
            }
            self.noise = self.noise.wrapping_mul(1664525).wrapping_add(1013904223);
            self.thump *= self.thump_coef;
            let hiss = (self.noise >> 8) as f32 * (2.0 / 16777216.0) - 1.0;
            self.thump_state += self.thump_pole * (hiss * self.thump - self.thump_state);
            self.click *= self.click_coef;
            let click = self.click_filter.tick(hiss * self.click, self.click_g, 0.7).0;
            let mono = 0.7 * sum + self.thump_state + click;
            // The sweep's notes get the amplifier's mid hump here; the vowel filter has its own formants.
            let hump = if self.voicing == Voicing::Sweep { 0.9 * bark } else { 0.0 };
            let mono = mono + hump * self.formant.tick(mono, self.formant_g, 1.0).0;
            // Every tine has its own pickup, so only the sum needs a limit.
            let mono = limit(mono);
            let mono = self.tube(mono);
            let (left, right) = self.ensemble.process(mono);
            self.tremolo_phase = self.tremolo_phase.wrapping_add(self.tremolo_step);
            let swing = self.sine.at(self.tremolo_phase);
            let (left, right) = (left * (1.0 - 0.5 * depth * (1.0 - swing)), right * (1.0 - 0.5 * depth * (1.0 + swing)));
            let (left, right) = self.phaser_fx.process(left, right);
            let (left, right) = self.echo.process(left, right);
            frame[0] = (left * self.volume).clamp(-1.0, 1.0);
            frame[1] = (right * self.volume).clamp(-1.0, 1.0);
        }
    }
}

/// Linear up to 0.5, then bending smoothly towards 0.95, so nothing reaches the final clamp at 1.
fn limit(x: f32) -> f32 {
    let over = x.abs() - 0.5;
    if over <= 0.0 { x } else { (0.5 + 0.45 * over / (0.45 + over)).copysign(x) }
}

/// One tine's pickup. A Rhodes tine swings across the face of a small magnet and coil, resting a little off its middle
/// (`PICKUP_OFFSET`); the flux through the coil is bell-shaped in the tine's position, `1 / (1 + (q - offset)^2)` with `q`
/// the swing in widths of the field. A small swing sees the slope: a clean copy of the tine. A hard swing passes the whole
/// bell twice a cycle and makes pulses, rich in harmonics up to the ninth and beyond (the bark), and the offset makes them
/// lopsided (the strong second harmonic, the growl). The coil reads the change of flux, which raises harmonic n n-fold
/// (the rasp), up to about the fifth: the coil's inductance and the amplifier stop it there.
///
/// The tine's own tone is already tuned, so it passes as it is, plus what the curve adds (the bend: the flux over its
/// slope at rest, minus the swing): 0.6 of it as it is (a hard swing's fundamental sags, as the tine runs past the field)
/// and, high-passed (so its fundamental does not fight the tine's) and differentiated, 1.6 of its change. A soft note is
/// therefore nearly a sine and a hard one barks, every note on its own pickup.
#[derive(Clone, Copy)]
struct Pickup {
    /// Field widths per unit of tine displacement (0: no pickup, the tine as it is).
    reach: f32,
    /// The rate over 2 pi times the pitch: a sample-to-sample difference as a derivative that keeps the pitch's level.
    deriv: f32,
    /// A second-order high-pass at 1.5 times the pitch, and its `tan` coefficient.
    high: Svf,
    high_g: f32,
    /// The last high-passed bend, and the differentiated bend low-passed at five times the pitch.
    last: f32,
    rasp: f32,
    rasp_pole: f32,
    /// The offset the lopsided curve leaves, followed at 20 Hz and taken off.
    dc: f32,
    dc_pole: f32,
}

impl Pickup {
    const OFF: Pickup = Pickup { reach: 0.0, deriv: 0.0, high: Svf::ZERO, high_g: 0.0, last: 0.0, rasp: 0.0, rasp_pole: 0.0, dc: 0.0, dc_pole: 0.0 };

    fn new(hz: f32, reach: f32, rate: f32) -> Pickup {
        Pickup {
            reach,
            deriv: rate / (2.0 * core::f32::consts::PI * hz),
            high: Svf::ZERO,
            high_g: tan_g((1.5 * hz).min(0.4 * rate), rate),
            last: 0.0,
            rasp: 0.0,
            rasp_pole: one_pole((5.0 * hz).min(0.4 * rate), rate),
            dc: 0.0,
            dc_pole: one_pole(20.0, rate),
        }
    }

    /// One sample of tine displacement `x` in, signal out; `scale` moves the tine closer (the mod wheel's bark).
    fn tick(&mut self, x: f32, scale: f32) -> f32 {
        let reach = self.reach * scale;
        if reach <= 0.0 {
            return x;
        }
        const REST: f32 = 1.0 / (1.0 + PICKUP_OFFSET * PICKUP_OFFSET);
        const SLOPE: f32 = 2.0 * PICKUP_OFFSET * REST * REST;
        let q = x * reach;
        let off = q - PICKUP_OFFSET;
        let bend = (1.0 / (1.0 + off * off) - REST) / SLOPE - q;
        let (band, low) = self.high.tick(bend, self.high_g, 1.4);
        let high = bend - 1.4 * band - low;
        self.rasp += self.rasp_pole * ((high - self.last) * self.deriv - self.rasp);
        self.last = high;
        // Part of the bend as it is: a hard swing runs past the field, so the fundamental sags and the harmonics take over.
        let y = x + (0.6 * bend + 1.6 * self.rasp) / reach;
        self.dc += self.dc_pole * (y - self.dc);
        y - self.dc
    }
}

fn one_pole(hz: f32, rate: f32) -> f32 {
    1.0 - math::exp64(-2.0 * core::f64::consts::PI * hz as f64 / rate as f64) as f32
}

#[cfg(test)]
mod tests;
