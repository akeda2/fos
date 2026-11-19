//! A logical mixer on top of a device's matrix mixer (tape ABI 41, `mixer_*`): stereo pairs, send levels in dB, pan and
//! mute, rendered to the device's mono crosspoints and output masters, and derived back from them.
//!
//! The device knows only mono crosspoints (output channel x source channel, sources being the inputs and then the playback
//! channels) and one master per output channel, as linear gains where `unity` is 0 dB. Pairs, pan and mute exist only
//! here. Ported from the KakelFix Linux mixer's model; `no_std` without a heap (tapes have none): fixed capacity of
//! `MAX_CHANNELS` per group, the maths from `dsp::math`.
#![no_std]

use core::ops::Deref;
use dsp::math::{ln64, pow10_64, sin_cos64, sqrt64};

/// The most channels per group (input, playback, output) the model holds; a device with more shows the first ones.
pub const MAX_CHANNELS: usize = 32;
/// Sources one output mixes: inputs and playback channels.
pub const MAX_SOURCES: usize = 2 * MAX_CHANNELS;

/// A short list with a fixed capacity (what `Vec` would be with a heap).
#[derive(Clone, Copy, Debug)]
pub struct List<T: Copy + Default, const N: usize> {
    items: [T; N],
    len: usize,
}

impl<T: Copy + Default, const N: usize> List<T, N> {
    pub fn new() -> Self {
        List { items: [T::default(); N], len: 0 }
    }

    pub fn push(&mut self, item: T) {
        if self.len < N {
            self.items[self.len] = item;
            self.len += 1;
        }
    }
}

impl<T: Copy + Default, const N: usize> Default for List<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Copy + Default, const N: usize> Deref for List<T, N> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        &self.items[..self.len]
    }
}

impl<T: Copy + Default + PartialEq, const N: usize, const M: usize> PartialEq<[T; M]> for List<T, N> {
    fn eq(&self, other: &[T; M]) -> bool {
        **self == other[..]
    }
}

impl<T: Copy + Default, const N: usize> FromIterator<T> for List<T, N> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut list = List::new();
        iter.into_iter().for_each(|item| list.push(item));
        list
    }
}

pub type Strips = List<Strip, MAX_SOURCES>;
pub type Cells = List<Cell, 4>;

pub const FADER_MIN_DB: f64 = -60.0;
pub const FADER_MAX_DB: f64 = 6.0;
const FRAC_PI_4: f64 = core::f64::consts::FRAC_PI_4;

pub fn db_to_lin(db: f64) -> f64 {
    if db.is_finite() { pow10_64(db / 20.0) } else { 0.0 }
}

pub fn lin_to_db(lin: f64) -> f64 {
    if lin > 0.0 { 20.0 * ln64(lin) / core::f64::consts::LN_10 } else { f64::NEG_INFINITY }
}

/// The arctangent of `x` >= 0, in [0, pi/2): range reduction to |t| <= tan(pi/8), then a series (error below 1e-10).
pub fn atan(x: f64) -> f64 {
    if x > 1.0 {
        return 2.0 * FRAC_PI_4 - atan(1.0 / x);
    }
    let (base, t) = if x > 0.414_213_562_373_095 { (FRAC_PI_4, (x - 1.0) / (x + 1.0)) } else { (0.0, x) };
    let t2 = t * t;
    let mut term = t;
    let mut sum = 0.0;
    for k in 0..12 {
        sum += term / (2 * k + 1) as f64;
        term *= -t2;
    }
    base + sum
}

/// One fader step from `db`. Stepping below the fader range turns the send off; stepping up from off starts at the bottom
/// of the range. Results snap to 0.1 dB.
pub fn step_db(db: f64, delta: f64) -> f64 {
    if !db.is_finite() {
        return if delta > 0.0 { FADER_MIN_DB } else { f64::NEG_INFINITY };
    }
    let value = round((db + delta) * 10.0) / 10.0;
    if value < FADER_MIN_DB { f64::NEG_INFINITY } else { value.min(FADER_MAX_DB) }
}

fn round(x: f64) -> f64 {
    dsp::math::floor64(x + 0.5)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Group {
    Input,
    Playback,
    Output,
}

impl Group {
    pub fn idx(self) -> usize {
        self as usize
    }
}

/// A mono channel or a stereo pair (`first`, `first + 1`) of one group.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Strip {
    pub group: Group,
    pub first: usize,
    pub stereo: bool,
}

impl Default for Strip {
    fn default() -> Self {
        Strip { group: Group::Input, first: 0, stereo: false }
    }
}

impl Strip {
    pub fn channels(&self) -> core::ops::Range<usize> {
        self.first..self.first + if self.stereo { 2 } else { 1 }
    }
}

/// How a mono source is spread over a stereo output, and how pan acts as balance otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PanLaw {
    /// sin/cos law, -3 dB per side at center.
    #[default]
    ConstantPower,
    /// Full level on both sides at center; the far side is turned down when panning.
    Balance,
}

impl PanLaw {
    pub fn name(self) -> &'static str {
        match self {
            PanLaw::ConstantPower => "-3 dB",
            PanLaw::Balance => "0 dB",
        }
    }
}

fn balance(pan: f64) -> (f64, f64) {
    ((1.0 - pan).min(1.0), (1.0 + pan).min(1.0))
}

fn balance_inv(l: f64, r: f64) -> (f64, f64) {
    let g = l.max(r);
    if g <= 0.0 {
        (0.0, 0.0)
    } else if r >= l {
        (g, 1.0 - l / g)
    } else {
        (g, r / g - 1.0)
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Send {
    pub db: f64,
    /// -1 = left, 0 = center, 1 = right.
    pub pan: f64,
    pub mute: bool,
    /// The device could not say what one of its crosspoints is (shown as "?" until set).
    pub unknown: bool,
}

impl Send {
    pub const OFF: Send = Send { db: f64::NEG_INFINITY, pan: 0.0, mute: false, unknown: false };
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Master {
    pub db: f64,
    pub mute: bool,
    pub unknown: bool,
}

impl Master {
    pub const OFF: Master = Master { db: f64::NEG_INFINITY, mute: false, unknown: false };
}

/// (output channel, source channel, raw gain). Source channels count inputs first, then playback.
pub type Cell = (usize, usize, u32);

/// The source number `render` uses for an output's master.
pub const MASTER_SOURCE: usize = usize::MAX;

#[derive(Clone, Debug)]
pub struct Mixer {
    /// Channel count per group (input, playback, output).
    pub n: [usize; 3],
    /// `paired[g][i]`: channel i (even) forms a stereo pair with i + 1.
    pub paired: [[bool; MAX_CHANNELS]; 3],
    pub pan_law: PanLaw,
    /// The device's 0 dB and largest gains.
    pub unity: u32,
    pub max: u32,
    pub master_max: u32,
    /// `sends[o * sources + s]` for the strip starting at output channel `o` and source channel `s`.
    sends: [Send; MAX_CHANNELS * MAX_SOURCES],
    masters: [Master; MAX_CHANNELS],
}

impl Mixer {
    /// A mixer with every channel paired with its neighbour (most interfaces are used in stereo pairs).
    /// Counts above `MAX_CHANNELS` are cut to it.
    pub fn new(n: [usize; 3], unity: u32, max: u32, master_max: u32) -> Self {
        let n = n.map(|count| count.min(MAX_CHANNELS));
        let mut mixer = Mixer {
            n,
            paired: [[false; MAX_CHANNELS]; 3],
            pan_law: PanLaw::default(),
            unity,
            max,
            master_max,
            sends: [Send::OFF; MAX_CHANNELS * MAX_SOURCES],
            masters: [Master::OFF; MAX_CHANNELS],
        };
        for group in [Group::Input, Group::Playback, Group::Output] {
            for first in (0..n[group.idx()]).step_by(2) {
                if first + 1 < n[group.idx()] {
                    mixer.paired[group.idx()][first] = true;
                }
            }
        }
        mixer
    }

    pub fn lin_to_raw(&self, lin: f64, max: u32) -> u32 {
        round(lin * self.unity as f64).clamp(0.0, max as f64) as u32
    }

    pub fn raw_to_lin(&self, raw: u32) -> f64 {
        raw as f64 / self.unity as f64
    }

    /// Toggles the pair containing channel `ch`. False if `ch` has no partner.
    pub fn toggle_pair(&mut self, g: Group, ch: usize) -> bool {
        let base = ch & !1;
        if base + 1 >= self.n[g.idx()] {
            return false;
        }
        let pair = &mut self.paired[g.idx()][base];
        *pair = !*pair;
        true
    }

    pub fn strips(&self, g: Group) -> Strips {
        let n = self.n[g.idx()];
        let mut strips = Strips::new();
        let mut i = 0;
        while i < n {
            let stereo = self.paired[g.idx()][i] && i + 1 < n;
            strips.push(Strip { group: g, first: i, stereo });
            i += if stereo { 2 } else { 1 };
        }
        strips
    }

    /// Input strips followed by playback strips.
    pub fn sources(&self) -> Strips {
        let mut strips = self.strips(Group::Input);
        self.strips(Group::Playback).iter().for_each(|&strip| strips.push(strip));
        strips
    }

    pub fn n_sources(&self) -> usize {
        self.n[0] + self.n[1]
    }

    /// The device's source channel number of a strip's channel `ch`.
    pub fn src_channel(&self, s: &Strip, ch: usize) -> usize {
        match s.group {
            Group::Input | Group::Output => ch,
            Group::Playback => self.n[0] + ch,
        }
    }

    fn key(&self, o: &Strip, s: &Strip) -> usize {
        o.first * MAX_SOURCES + self.src_channel(s, s.first)
    }

    pub fn send(&self, o: &Strip, s: &Strip) -> Send {
        self.sends[self.key(o, s)]
    }

    /// Sets a send; the crosspoints to write to the device.
    pub fn set_send(&mut self, o: &Strip, s: &Strip, v: Send) -> Cells {
        let key = self.key(o, s);
        self.sends[key] = Send { unknown: false, ..v };
        self.send_cells(o, s, &v)
    }

    /// The crosspoints one send covers. Every (output, source) channel combination belongs to exactly one send, so rendering
    /// all sends sets the whole matrix.
    pub fn send_cells(&self, o: &Strip, s: &Strip, v: &Send) -> Cells {
        let g = if v.mute { 0.0 } else { db_to_lin(v.db) };
        let o0 = o.first;
        let s0 = self.src_channel(s, s.first);
        let raw = |x: f64| self.lin_to_raw(x, self.max);
        match (s.stereo, o.stereo) {
            (false, false) => [(o0, s0, raw(g))].into_iter().collect(),
            (false, true) => {
                let (l, r) = match self.pan_law {
                    PanLaw::ConstantPower => {
                        let (sin, cos) = sin_cos64((v.pan + 1.0) * FRAC_PI_4);
                        (cos, sin)
                    }
                    PanLaw::Balance => balance(v.pan),
                };
                [(o0, s0, raw(g * l)), (o0 + 1, s0, raw(g * r))].into_iter().collect()
            }
            (true, false) => [(o0, s0, raw(g * 0.5)), (o0, s0 + 1, raw(g * 0.5))].into_iter().collect(),
            (true, true) => {
                let (l, r) = balance(v.pan);
                [(o0, s0, raw(g * l)), (o0 + 1, s0 + 1, raw(g * r)), (o0, s0 + 1, 0), (o0 + 1, s0, 0)].into_iter().collect()
            }
        }
    }

    pub fn master(&self, o: &Strip) -> Master {
        self.masters[o.first]
    }

    /// Sets an output strip's master; the (output channel, gain) pairs to write.
    pub fn set_master(&mut self, o: &Strip, v: Master) -> List<(usize, u32), 2> {
        self.masters[o.first] = Master { unknown: false, ..v };
        let raw = if v.mute { 0 } else { self.lin_to_raw(db_to_lin(v.db), self.master_max) };
        o.channels().map(|c| (c, raw)).collect()
    }

    /// Every crosspoint (output, source, raw gain) and master (output, MASTER_SOURCE, raw gain) of the whole logical state.
    pub fn render(&self, mut cell: impl FnMut(usize, usize, u32)) {
        for o in self.strips(Group::Output).iter() {
            for s in self.sources().iter() {
                for &(oc, sc, raw) in self.send_cells(o, s, &self.send(o, s)).iter() {
                    cell(oc, sc, raw);
                }
            }
            let master = self.master(o);
            let raw = if master.mute { 0 } else { self.lin_to_raw(db_to_lin(master.db), self.master_max) };
            for c in o.channels() {
                cell(c, MASTER_SOURCE, raw);
            }
        }
    }

    /// Rebuilds sends and masters from the device's gains (`None`: unknown) for the current pairing. Mute is lost (a muted
    /// send reads as off); cells a send cannot express are ignored.
    pub fn derive(&mut self, gain: impl Fn(usize, usize) -> Option<u32>, master: impl Fn(usize) -> Option<u32>) {
        self.sends.iter_mut().for_each(|send| *send = Send::OFF);
        self.masters.iter_mut().for_each(|m| *m = Master::OFF);
        let unity = self.unity as f64;
        let lin = |raw: Option<u32>| raw.map(|raw| raw as f64 / unity);
        for o in self.strips(Group::Output).iter().copied() {
            for s in self.sources().iter().copied() {
                let o0 = o.first;
                let s0 = self.src_channel(&s, s.first);
                let cells = match (s.stereo, o.stereo) {
                    (false, false) => [lin(gain(o0, s0)), Some(0.0)],
                    (false, true) => [lin(gain(o0, s0)), lin(gain(o0 + 1, s0))],
                    (true, false) => [lin(gain(o0, s0)), lin(gain(o0, s0 + 1))],
                    (true, true) => [lin(gain(o0, s0)), lin(gain(o0 + 1, s0 + 1))],
                };
                let key = self.key(&o, &s);
                let (Some(c0), Some(c1)) = (cells[0], cells[1]) else {
                    self.sends[key] = Send { unknown: true, ..Send::OFF };
                    continue;
                };
                let c = [c0, c1];
                let (g, pan) = match (s.stereo, o.stereo) {
                    (false, false) => (c[0], 0.0),
                    (false, true) => match self.pan_law {
                        PanLaw::ConstantPower => {
                            let g = sqrt64(c[0] * c[0] + c[1] * c[1]);
                            let angle = if c[0] > 0.0 { atan(c[1] / c[0]) } else { 2.0 * FRAC_PI_4 };
                            (g, if g > 0.0 { angle / FRAC_PI_4 - 1.0 } else { 0.0 })
                        }
                        PanLaw::Balance => balance_inv(c[0], c[1]),
                    },
                    (true, false) => (c[0] + c[1], 0.0),
                    (true, true) => balance_inv(c[0], c[1]),
                };
                if g > 0.0 {
                    self.sends[key] = Send { db: lin_to_db(g), pan, mute: false, unknown: false };
                }
            }
            let gains: List<Option<u32>, 2> = o.channels().map(&master).collect();
            self.masters[o.first] = if gains.iter().any(Option::is_none) {
                Master { unknown: true, ..Master::OFF }
            } else {
                let g = gains.iter().flatten().map(|&raw| raw as f64 / unity).fold(0.0, f64::max);
                Master { db: lin_to_db(g), mute: false, unknown: false }
            };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const UNITY: u32 = 0x8000;

    fn mixer() -> Mixer {
        Mixer::new([4, 4, 4], UNITY, 0x10000, 0x10000)
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn db_round_trip_and_atan() {
        assert!(close(db_to_lin(0.0), 1.0));
        assert!(close(lin_to_db(db_to_lin(-12.5)), -12.5));
        assert_eq!(db_to_lin(f64::NEG_INFINITY), 0.0);
        for x in [0.0, 0.2, 0.5, 1.0, 2.0, 10.0] {
            let expected = [0.0, 0.197_395_559_849_881, 0.463_647_609_000_806, FRAC_PI_4, 1.107_148_717_794_09, 1.471_127_674_303_73];
            let i = [0.0, 0.2, 0.5, 1.0, 2.0, 10.0].iter().position(|&v| v == x).unwrap();
            assert!(close(atan(x), expected[i]), "atan({x})");
        }
    }

    #[test]
    fn steps_snap_and_turn_off() {
        assert_eq!(step_db(f64::NEG_INFINITY, 1.0), FADER_MIN_DB);
        assert_eq!(step_db(FADER_MIN_DB, -1.0), f64::NEG_INFINITY);
        assert_eq!(step_db(5.5, 1.0), FADER_MAX_DB);
        assert!(close(step_db(-3.04, 0.5), -2.5));
    }

    #[test]
    fn mono_to_stereo_constant_power_round_trips() {
        let mut m = mixer();
        m.toggle_pair(Group::Input, 0); // inputs 0 and 1 mono
        let out = m.strips(Group::Output)[0];
        let input = m.strips(Group::Input)[0];
        assert!(!input.stereo && out.stereo);
        let cells = m.set_send(&out, &input, Send { db: 0.0, pan: 0.0, mute: false, unknown: false });
        // Center: -3 dB on each side.
        let side = (UNITY as f64 * core::f64::consts::FRAC_1_SQRT_2).round() as u32;
        assert_eq!(cells, [(0, 0, side), (1, 0, side)]);
        let cells = m.set_send(&out, &input, Send { db: -6.0, pan: 0.5, mute: false, unknown: false });
        let mut matrix = [[0u32; 8]; 4];
        m.render(|o, s, raw| {
            if s != MASTER_SOURCE {
                matrix[o][s] = raw;
            }
        });
        assert_eq!(matrix[0][0], cells[0].2);
        let mut back = m.clone();
        back.derive(|o, s| Some(matrix[o][s]), |_| Some(UNITY));
        let send = back.send(&out, &input);
        assert!((send.db + 6.0).abs() < 0.01 && (send.pan - 0.5).abs() < 0.01, "{send:?}");
    }

    #[test]
    fn stereo_pair_to_pair_clears_cross_terms() {
        let mut m = mixer();
        let out = m.strips(Group::Output)[1];
        let play = m.strips(Group::Playback)[0];
        let cells = m.set_send(&out, &play, Send { db: 0.0, pan: 0.0, mute: false, unknown: false });
        // Output channels 2/3, playback channels are sources 4/5.
        assert_eq!(cells, [(2, 4, UNITY), (3, 5, UNITY), (2, 5, 0), (3, 4, 0)]);
    }

    #[test]
    fn unknown_crosspoints_and_masters_derive_as_unknown() {
        let mut m = mixer();
        m.derive(|o, s| if o == 0 && s == 0 { None } else { Some(0) }, |o| if o == 2 { None } else { Some(UNITY) });
        let outs = m.strips(Group::Output);
        let input = m.strips(Group::Input)[0];
        assert!(m.send(&outs[0], &input).unknown);
        assert!(!m.send(&outs[1], &input).unknown);
        assert!(m.master(&outs[1]).unknown);
        assert!(close(m.master(&outs[0]).db, 0.0));
        // Setting it makes it known.
        m.set_send(&outs[0], &input, Send::OFF);
        assert!(!m.send(&outs[0], &input).unknown);
    }

    #[test]
    fn gains_clamp_to_the_device_maximum() {
        let mut m = mixer();
        let out = m.strips(Group::Output)[0];
        let cells = m.set_master(&out, Master { db: 12.0, mute: false, unknown: false });
        assert_eq!(cells, [(0, 0x10000), (1, 0x10000)]);
    }
}
