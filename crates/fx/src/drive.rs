//! Overdrive in the flavours of the amplifiers the instruments were played through. Each is a static curve (with a bias for
//! the asymmetric ones) after a pre-gain of 1 to 8 (`amount` 0..=1), run at 4x the sample rate so what it adds above Nyquist
//! does not fold back (`oversample.rs`), and scaled up by `1 + amount` afterwards so it does not sound quieter when it
//! saturates. They are models of how each kind behaves, not circuit simulations.
//!
//! | kind | models | sound |
//! | --- | --- | --- |
//! | Soft | the original `x / (1 + |x|)` | gentle, a little fizzy on top (a kink at zero) |
//! | Tanh | a smooth valve stage | round, few high harmonics |
//! | Triode | a 12AX7 preamp stage | tanh biased off centre: even harmonics, a DC blocker after |
//! | PushPull | a class AB power stage (6L6, 6550, EL34: Leslie and Rhodes amps) | clean up to a firm knee, then flat: `x / (1 + x^4)^(1/4)` |
//! | Sag | a tube amp with a soft power supply | tanh whose gain drops while it is loud and recovers in 0.1 s: compression |
//! | Transistor | a hard-clipping transistor or diode stage | clamps at +-1: bright and buzzy |
//! | Vox | a Vox AC30: Class A EL84 pair, top boost | treble lifted before a biased, quicker-saturating curve: chime |

use dsp::math;

use crate::oversample::Stage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveKind {
    Soft,
    Tanh,
    Triode,
    PushPull,
    Sag,
    Transistor,
    Vox,
}

impl DriveKind {
    pub const ALL: [DriveKind; 7] =
        [DriveKind::Soft, DriveKind::Tanh, DriveKind::Triode, DriveKind::PushPull, DriveKind::Sag, DriveKind::Transistor, DriveKind::Vox];

    pub fn name(self) -> &'static str {
        ["Soft", "Tanh", "Triode", "Push-pull", "Sag", "Transistor", "Vox AC30"][self as usize]
    }

    /// A label for a small button.
    pub fn short(self) -> &'static str {
        ["Soft", "Tanh", "Triode", "P-P", "Sag", "Trans", "Vox"][self as usize]
    }

    /// The offset added to the input before the pre-gain: asymmetry gives even harmonics.
    fn bias(self, amount: f32) -> f32 {
        match self {
            DriveKind::Triode => 0.25 * amount,
            DriveKind::Vox => 0.15 * amount,
            _ => 0.0,
        }
    }

    /// The static curve, -1..=1 (the pre-gain has been applied).
    fn shape(self, x: f32) -> f32 {
        match self {
            DriveKind::Soft => x / (1.0 + x.abs()),
            DriveKind::Tanh | DriveKind::Triode | DriveKind::Sag => math::tanh(x),
            DriveKind::PushPull => {
                let x2 = x * x;
                x / math::sqrt(math::sqrt(1.0 + x2 * x2))
            }
            DriveKind::Transistor => x.clamp(-1.0, 1.0),
            DriveKind::Vox => x / math::sqrt(1.0 + x * x),
        }
    }

    fn asymmetric(self) -> bool {
        matches!(self, DriveKind::Triode | DriveKind::Vox)
    }
}

/// An overdrive of the chosen `DriveKind`, 4x oversampled. Amount 0 is a bypass (no filtering, no delay); the filters add a
/// latency of a few dozen samples while it is on.
pub struct Overdrive {
    outer: Stage<47>,
    inner: Stage<15>,
    kind: DriveKind,
    shelf: f32,
    shelf_pole: f32,
    sag: f32,
    sag_attack: f32,
    sag_release: f32,
    dc_in: f32,
    dc_out: f32,
    dc_pole: f32,
}

impl Overdrive {
    /// A soft overdrive at 48 kHz.
    pub fn new() -> Self {
        let mut overdrive = Overdrive {
            outer: Stage::new(),
            inner: Stage::new(),
            kind: DriveKind::Soft,
            shelf: 0.0,
            shelf_pole: 0.0,
            sag: 0.0,
            sag_attack: 0.0,
            sag_release: 0.0,
            dc_in: 0.0,
            dc_out: 0.0,
            dc_pole: 0.0,
        };
        overdrive.set_sample_rate(48000);
        overdrive
    }

    pub fn set_sample_rate(&mut self, sample_rate: u32) {
        let rate = sample_rate.clamp(8000, 192000) as f64;
        let pole = |hz: f64| 1.0 - math::exp64(-2.0 * core::f64::consts::PI * hz / rate) as f32;
        let time = |seconds: f64| 1.0 - math::exp64(-1.0 / (seconds * rate)) as f32;
        self.shelf_pole = pole(1500.0);
        self.sag_attack = time(0.015);
        self.sag_release = time(0.12);
        self.dc_pole = 1.0 - pole(10.0);
    }

    pub fn kind(&self) -> DriveKind {
        self.kind
    }

    pub fn set_kind(&mut self, kind: DriveKind) {
        self.kind = kind;
    }

    pub fn process(&mut self, sample: f32, amount: f32) -> f32 {
        if amount <= 0.0 {
            return sample;
        }
        let kind = self.kind;
        let mut input = sample;
        if kind == DriveKind::Vox {
            // The top boost: a lift above 1.5 kHz ahead of the saturation, so the highs break up first.
            self.shelf += self.shelf_pole * (input - self.shelf);
            input += 0.8 * (input - self.shelf);
        }
        let mut gain = 1.0 + 7.0 * amount;
        if kind == DriveKind::Sag {
            gain /= 1.0 + 2.0 * amount * self.sag;
        }
        let bias = kind.bias(amount);
        let offset = kind.shape(bias * gain);
        let output = self.outer.process(input, |x| self.inner.process(x, |y| kind.shape((y + bias) * gain) - offset));
        if kind == DriveKind::Sag {
            let level = output.abs();
            self.sag += (level - self.sag) * if level > self.sag { self.sag_attack } else { self.sag_release };
        }
        let mut output = output * (1.0 + amount);
        if kind.asymmetric() {
            // The curve's mean is not zero when it is driven off centre; take it out below about 10 Hz.
            let blocked = output - self.dc_in + self.dc_pole * self.dc_out;
            self.dc_in = output;
            self.dc_out = blocked;
            output = blocked;
        }
        output
    }
}

impl Default for Overdrive {
    fn default() -> Self {
        Self::new()
    }
}
