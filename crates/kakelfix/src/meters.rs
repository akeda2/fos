//! The Meters page's settings: which scale the bars show (sample peak in dBFS, the Nordic PPM, or EBU R128 loudness per
//! input pair) and which input is selected (its gain is what Up and Down change). The readings themselves are the tape's
//! (`audio_screen::meters`, `dsp::loudness`); `generation` tells it to clear the holds, clip lamps and loudness.

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MeterScale {
    Dbfs,
    Nordic,
    Lufs,
}

impl MeterScale {
    pub fn name(self) -> &'static str {
        match self {
            MeterScale::Dbfs => "dBFS",
            MeterScale::Nordic => "Nordic",
            MeterScale::Lufs => "LUFS",
        }
    }
}

/// Config-file words for the scales, in the enum's order.
const SCALE_NAMES: [&str; 3] = ["dbfs", "nordic", "lufs"];

#[derive(Clone, Copy, Debug)]
pub struct Metering {
    pub scale: MeterScale,
    /// The selected capture channel, 0-based; in LUFS its pair (`input / 2`) is the one read out.
    pub input: usize,
    /// Capture channels; set by the tape.
    pub channels: usize,
    pub generation: u32,
}

impl Metering {
    pub const fn new() -> Self {
        Self { scale: MeterScale::Nordic, input: 0, channels: 2, generation: 0 }
    }

    pub fn next_scale(&mut self) {
        self.scale = match self.scale {
            MeterScale::Dbfs => MeterScale::Nordic,
            MeterScale::Nordic => MeterScale::Lufs,
            MeterScale::Lufs => MeterScale::Dbfs,
        };
    }

    /// Inputs per bar: one, or a stereo pair in LUFS.
    pub fn per_bar(&self) -> usize {
        if self.scale == MeterScale::Lufs { 2 } else { 1 }
    }

    /// Bars shown: one per input, or one per pair in LUFS.
    pub fn bars(&self) -> usize {
        self.channels.max(1).div_ceil(self.per_bar())
    }

    /// The bar the selected input is in.
    pub fn bar(&self) -> usize {
        self.input / self.per_bar()
    }

    pub fn step(&mut self, forward: bool) {
        let last = self.channels.max(1) - 1;
        self.input = if forward { (self.input + 1).min(last) } else { self.input.saturating_sub(1) };
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn persisted(&self) -> (MeterScale, usize) {
        (self.scale, self.input)
    }

    /// Takes `mt-scale=dbfs|nordic|lufs` and `mt-input=N` from `value(key)`; unknown values are ignored.
    pub fn load<'a>(&mut self, value: impl Fn(&str) -> Option<&'a str>) {
        if let Some(i) = value("mt-scale").and_then(|v| SCALE_NAMES.iter().position(|name| *name == v)) {
            self.scale = [MeterScale::Dbfs, MeterScale::Nordic, MeterScale::Lufs][i];
        }
        if let Some(input) = value("mt-input").and_then(|v| v.parse::<usize>().ok()) {
            self.input = input.saturating_sub(1).min(self.channels.max(1) - 1);
        }
    }

    pub fn save(&self, mut put: impl FnMut(&str, core::fmt::Arguments)) {
        put("mt-scale", format_args!("{}", SCALE_NAMES[self.scale as usize]));
        put("mt-input", format_args!("{}", self.input + 1));
    }
}

impl Default for Metering {
    fn default() -> Self {
        Self::new()
    }
}

/// The kinds of per-input control shown under a meter, by the words of a control's name before its trailing number
/// ("Mic gain 3", "+48V 1", "Inst 2", "Level 5"): gains, phantom power, pads, instrument and level switches. Other
/// numbered controls (a loopback switch per output pair) are not an input's.
const INPUT_WORDS: [&str; 6] = ["gain", "48V", "Level", "Inst", "Pad", "AutoSet"];

/// The input number (1-based) a control called `name` belongs to, if it is one of an input's controls.
pub fn input_of(name: &str) -> Option<usize> {
    let (label, number) = name.rsplit_once(' ')?;
    let number = number.parse::<usize>().ok().filter(|&n| n > 0)?;
    INPUT_WORDS.iter().any(|word| label.contains(word)).then_some(number)
}

/// A control's label without its number ("+48V 1" -> "+48V").
pub fn short_label(name: &str) -> &str {
    match name.rsplit_once(' ') {
        Some((label, number)) if number.parse::<usize>().is_ok() => label,
        _ => name,
    }
}
