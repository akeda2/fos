//! The Meters page's settings: which scale the bars show (sample peak in dBFS, the Nordic PPM, or EBU R128 loudness per
//! input pair), which inputs have a bar (`shown`) and which input is selected (its gain is what Up and Down change). The readings themselves are the tape's
//! (`audio_screen::meters`, `dsp::loudness`); `generation` tells it to clear the holds, clip lamps and loudness.

use core::fmt::Write;

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
    /// Bit i: input i has a bar. Never empty within `channels` (`fix`).
    pub shown: u64,
    /// Inputs cabled back from an output (`mt-loop`): the output's 1-based number, 0 for an ordinary input.
    pub loops: [u8; 64],
    pub generation: u32,
}

impl Metering {
    pub const fn new() -> Self {
        Self { scale: MeterScale::Nordic, input: 0, channels: 2, shown: u64::MAX, loops: [0; 64], generation: 0 }
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

    /// Every input of the capture, as a `shown` mask.
    pub fn all(&self) -> u64 {
        if self.channels >= 64 { u64::MAX } else { (1u64 << self.channels.max(1)) - 1 }
    }

    pub fn is_shown(&self, input: usize) -> bool {
        input < 64 && self.shown & self.all() & 1 << input != 0
    }

    /// Shows `mask` (inputs past the capture dropped; nothing at all means everything) and keeps the selection on a shown
    /// input.
    pub fn show(&mut self, mask: u64) {
        self.shown = if mask & self.all() == 0 { self.all() } else { mask & self.all() };
        self.fix();
    }

    /// Moves the selection onto a shown input: the next one up, else the last.
    pub fn fix(&mut self) {
        if self.shown & self.all() == 0 {
            self.shown = self.all();
        }
        if !self.is_shown(self.input) {
            let channels = self.channels.max(1);
            self.input = (self.input..channels).find(|&i| self.is_shown(i)).or_else(|| (0..channels).rev().find(|&i| self.is_shown(i))).unwrap_or(0);
        }
    }

    /// Whether bar-unit `unit` (an input, or a pair in LUFS) has a bar: any of its inputs is shown.
    fn unit_shown(&self, unit: usize) -> bool {
        let per = self.per_bar();
        (unit * per..((unit + 1) * per).min(self.channels.max(1))).any(|i| self.is_shown(i))
    }

    /// Bars shown: one per shown input, or one per pair with a shown input in LUFS.
    pub fn bars(&self) -> usize {
        (0..self.channels.max(1).div_ceil(self.per_bar())).filter(|&u| self.unit_shown(u)).count()
    }

    /// The first input of bar `bar` (in LUFS the pair's left input), if there is such a bar.
    pub fn bar_input(&self, bar: usize) -> Option<usize> {
        let per = self.per_bar();
        (0..self.channels.max(1).div_ceil(per)).filter(|&u| self.unit_shown(u)).nth(bar).map(|u| u * per)
    }

    /// The bar `input` is shown in.
    pub fn bar_of(&self, input: usize) -> Option<usize> {
        let unit = input / self.per_bar();
        self.unit_shown(unit).then(|| (0..unit).filter(|&u| self.unit_shown(u)).count())
    }

    /// The bar the selected input is in.
    pub fn bar(&self) -> usize {
        self.bar_of(self.input).unwrap_or(0)
    }

    /// The next (or previous) shown input; stays at the ends.
    pub fn step(&mut self, forward: bool) {
        let next = if forward { (self.input + 1..self.channels.max(1)).find(|&i| self.is_shown(i)) } else { (0..self.input).rev().find(|&i| self.is_shown(i)) };
        self.input = next.unwrap_or(self.input);
    }

    /// The first or last shown input.
    pub fn home(&mut self, end: bool) {
        self.input = 0;
        self.fix();
        if end {
            self.input = (0..self.channels.max(1)).rev().find(|&i| self.is_shown(i)).unwrap_or(0);
        }
    }

    pub fn reset(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    pub fn persisted(&self) -> (MeterScale, usize, u64) {
        (self.scale, self.input, self.shown & self.all())
    }

    /// Takes `mt-scale=dbfs|nordic|lufs`, `mt-input=N` and `mt-show=all|1-4,9,10` from `value(key)`; unknown values
    /// are ignored.
    pub fn load<'a>(&mut self, value: impl Fn(&str) -> Option<&'a str>) {
        if let Some(i) = value("mt-scale").and_then(|v| SCALE_NAMES.iter().position(|name| *name == v)) {
            self.scale = [MeterScale::Dbfs, MeterScale::Nordic, MeterScale::Lufs][i];
        }
        if let Some(input) = value("mt-input").and_then(|v| v.parse::<usize>().ok()) {
            self.input = input.saturating_sub(1).min(self.channels.max(1) - 1);
        }
        if let Some(list) = value("mt-show") {
            let mut mask = 0u64;
            for part in list.split(',') {
                let (from, to) = part.split_once('-').unwrap_or((part, part));
                if let (Ok(from), Ok(to)) = (from.trim().parse::<usize>(), to.trim().parse::<usize>()) {
                    for n in from.max(1)..=to.min(64) {
                        mask |= 1 << (n - 1);
                    }
                }
            }
            if list == "all" { self.show(self.all()) } else { self.show(mask) }
        }
        if let Some(list) = value("mt-loop") {
            self.loops = [0; 64];
            // "IN:OUT" or "FROM-TO:OUT", the inputs carrying OUT, OUT+1, ...
            for part in list.split(',') {
                let Some((inputs, output)) = part.split_once(':') else { continue };
                let (from, to) = inputs.split_once('-').unwrap_or((inputs, inputs));
                let (Ok(from), Ok(to), Ok(output)) = (from.trim().parse::<usize>(), to.trim().parse::<usize>(), output.trim().parse::<usize>()) else { continue };
                for n in from.max(1)..=to.min(64) {
                    self.loops[n - 1] = (output + n - from).min(255) as u8;
                }
            }
        }
        self.fix();
    }

    pub fn save(&self, mut put: impl FnMut(&str, core::fmt::Arguments)) {
        put("mt-scale", format_args!("{}", SCALE_NAMES[self.scale as usize]));
        put("mt-input", format_args!("{}", self.input + 1));
        let mut list = crate::Text::<160>::new();
        if self.shown & self.all() == self.all() {
            let _ = list.write_str("all");
        } else {
            let mut n = 0;
            while n < self.channels {
                if !self.is_shown(n) {
                    n += 1;
                    continue;
                }
                let start = n;
                while n + 1 < self.channels && self.is_shown(n + 1) {
                    n += 1;
                }
                let sep = if list.is_empty() { "" } else { "," };
                let _ = if start == n { write!(list, "{sep}{}", n + 1) } else { write!(list, "{sep}{}-{}", start + 1, n + 1) };
                n += 1;
            }
        }
        put("mt-show", format_args!("{}", list.as_str()));
        list.clear();
        let mut n = 0;
        while n < 64 {
            let output = self.loops[n];
            if output == 0 {
                n += 1;
                continue;
            }
            let start = n;
            while n + 1 < 64 && self.loops[n + 1] != 0 && self.loops[n + 1] as usize == self.loops[n] as usize + 1 {
                n += 1;
            }
            let sep = if list.is_empty() { "" } else { "," };
            let _ = if start == n { write!(list, "{sep}{}:{output}", n + 1) } else { write!(list, "{sep}{}-{}:{output}", start + 1, n + 1) };
            n += 1;
        }
        if !list.is_empty() {
            put("mt-loop", format_args!("{}", list.as_str()));
        }
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
