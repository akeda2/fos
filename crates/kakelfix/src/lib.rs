//! KakelFix for Fosfor OS: the state, actions, layout and pointer handling of the `kakelfix` tape, kept apart from drawing
//! so the host can test them. The tape reaches the active audio device's mixer (tape ABI 41, `mixer_*`) through the
//! `Device` trait; `mixer_model` turns its mono crosspoints into stereo strips with dB faders, pan and mute.
//!
//! Pages (chosen by the soft keys down the right edge): Mixer, Recorder (a later milestone), Analyzer and Meters. The soft keys
//! follow the page: the active page's views sit indented under it, its actions below, Quit last. The Mixer page has three
//! views: Outputs (one output's mix as fader strips, its master at the end), Matrix (every send of every output as a
//! table) and Device (the device's own settings). The Analyzer shows a captured channel pair as a waveform, a spectrum,
//! or both (`analyzer`). Meters shows every captured input as a bar (dBFS, Nordic PPM or LUFS per pair) with that
//! input's gain and switches under it (`meters`).
#![no_std]

pub mod analyzer;
pub mod layout;
pub mod meters;
pub mod queue;

use core::fmt::Write;
use fosfor_abi::tape::{
    MixerControl, MIXER_CONTROL_ACTION, MIXER_CONTROL_BOOL, MIXER_CONTROL_ENUM, MIXER_FLAG_HAZARD, MIXER_FLAG_READONLY, MIXER_FLAG_UNKNOWN,
};
pub use analyzer::{AnView, Analysis, Persisted, WaveMode};
pub use layout::{Hit, Layout, Rect};
pub use meters::{MeterScale, Metering};
pub use queue::{Link, Queued};
use mixer_model::{Cell, Group, List, Master, Mixer, PanLaw, Send, Strip, Strips, MAX_CHANNELS};
pub use mixer_model::{FADER_MAX_DB, FADER_MIN_DB};

/// What the tape needs from the device's mixer. Channel numbers are the device's: sources count inputs, then playback.
pub trait Device {
    fn gain(&self, output: usize, source: usize) -> Option<u32>;
    fn set_gain(&mut self, output: usize, source: usize, raw: u32) -> bool;
    fn master(&self, output: usize) -> Option<u32>;
    fn set_master(&mut self, output: usize, raw: u32) -> bool;
    /// Changes whenever any gain or control changes, whoever changed it.
    fn serial(&self) -> u64;
    /// A channel's name into `out`; its length (0: no name).
    fn name(&self, group: Group, index: usize, out: &mut [u8]) -> usize;
    fn control(&self, index: usize) -> Option<MixerControl>;
    fn control_item(&self, index: usize, item: usize, out: &mut [u8]) -> usize;
    fn set_control(&mut self, index: usize, value: i64) -> bool;
}

/// The device's mixer as `mixer_info` describes it.
#[derive(Clone, Copy, Debug)]
pub struct Shape {
    /// Inputs, playback channels, outputs.
    pub counts: [usize; 3],
    pub unity: u32,
    pub max: u32,
    pub master_max: u32,
    pub masters: bool,
    pub controls: usize,
}

/// A short text with a fixed capacity; writes past the end are cut at a character boundary.
#[derive(Clone, Copy)]
pub struct Text<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Text<N> {
    pub const fn new() -> Self {
        Text { bytes: [0; N], len: 0 }
    }

    pub fn from(text: &str) -> Self {
        let mut out = Self::new();
        let _ = out.write_str(text);
        out
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for Text<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Write for Text<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for c in text.chars() {
            let mut buffer = [0u8; 4];
            let bytes = c.encode_utf8(&mut buffer).as_bytes();
            if self.len + bytes.len() > N {
                break;
            }
            self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
            self.len += bytes.len();
        }
        Ok(())
    }
}

/// A zero-padded UTF-8 field (`MixerControl::name`, `unit`) as text.
pub fn field(bytes: &[u8]) -> &str {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).unwrap_or("")
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    Mixer,
    Recorder,
    Analyzer,
    Meters,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Outputs,
    Matrix,
    Device,
}

/// What a key (or a soft key) asks for; the tape maps its keys onto these.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    /// A send or master up or down by this many dB.
    Step(f64),
    Unity,
    Off,
    Mute,
    Pan(f64),
    Center,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    NextOutput,
    PrevOutput,
    /// Pair or split the selected source (on the master strip: the output).
    Pair,
    PairOutput,
    PanLaw,
    NextView,
    Page(Page),
    View(View),
    /// Analyzer: a view, freeze, envelope or scope, reset what was gathered, dB or linear waveform, spectrum resolution.
    AnView(AnView),
    Freeze,
    WaveMode,
    Reset,
    WaveScale,
    Resolution,
    /// Meters: the next scale, the selected input's phantom power, its instrument (or other) switch.
    MeterScale,
    Phantom,
    InputSwitch,
    Enter,
    Yes,
    No,
    Quit,
}

/// What a held mouse button or finger is moving.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Grab {
    /// The fader of strip n (sources, then the master).
    Fader(usize),
    Pan(usize),
    /// A control under a meter, dragged up or down from where it was pressed (`y`) and what it was (`value`).
    Control { index: usize, y: i32, value: i64 },
}

/// Soft-key slots down the right edge: `SOFT_NAV` for the pages with the active page's views under it, then
/// `SOFT_ACTIONS` for that page's actions, then Quit, with half a key between the groups.
pub const SOFT_SLOTS: usize = 11;
pub const SOFT_NAV: usize = 7;
pub const SOFT_ACTIONS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SoftKind {
    Empty,
    Page,
    /// One of the active page's views, drawn indented under it.
    View,
    Action,
}

#[derive(Clone, Copy, Debug)]
pub struct Soft {
    pub label: &'static str,
    pub kind: SoftKind,
    pub action: Action,
    /// The page or view shown, or an action that is on (Freeze).
    pub active: bool,
}

impl Soft {
    const EMPTY: Soft = Soft { label: "", kind: SoftKind::Empty, action: Action::Quit, active: false };
}

#[derive(Clone, Copy)]
enum Target {
    Send(Strip, Strip),
    Master(Strip),
}

pub struct App {
    pub mix: Mixer,
    pub shape: Shape,
    pub page: Page,
    pub view: View,
    /// Outputs view: the output strip shown, and the strip selected (sources, then the master).
    pub out_sel: usize,
    pub src_sel: usize,
    /// Matrix view: the source row and output column.
    pub row: usize,
    pub col: usize,
    pub ctl_sel: usize,
    /// The first strip, tab, matrix row and column and control shown (scrolling keeps the selection visible).
    pub first_strip: usize,
    pub first_tab: usize,
    pub first_row: usize,
    pub first_col: usize,
    pub first_ctl: usize,
    pub first_meter: usize,
    /// A hazardous control change waiting for yes or no: (control, value).
    pub confirm: Option<(usize, i64)>,
    /// Whether switching a hazardous control on (phantom power) asks first. Off by default; the Device page's last row
    /// toggles it and the tape saves it (`ask_changed`). Hazardous actions (a mixer reset) always ask.
    pub ask_hazard: bool,
    /// Set when `ask_hazard` was toggled, for the tape to save; it clears it.
    pub ask_changed: bool,
    pub controls: List<MixerControl, 32>,
    pub analysis: Analysis,
    pub metering: Metering,
    pub message: Text<72>,
    pub quit: bool,
    names: [[Text<16>; MAX_CHANNELS]; 3],
    seen: u64,
}

impl App {
    pub fn new(shape: Shape, device: &mut impl Device) -> App {
        let mix = Mixer::new(shape.counts, shape.unity, shape.max, shape.master_max);
        let mut app = App {
            mix,
            shape,
            page: Page::Mixer,
            view: View::Outputs,
            out_sel: 0,
            src_sel: 0,
            row: 0,
            col: 0,
            ctl_sel: 0,
            first_strip: 0,
            first_tab: 0,
            first_row: 0,
            first_col: 0,
            first_ctl: 0,
            first_meter: 0,
            confirm: None,
            ask_hazard: false,
            ask_changed: false,
            controls: List::new(),
            analysis: Analysis::new(),
            metering: Metering::new(),
            message: Text::new(),
            quit: false,
            names: [[Text::new(); MAX_CHANNELS]; 3],
            seen: 0,
        };
        for group in [Group::Input, Group::Playback, Group::Output] {
            for index in 0..app.mix.n[group.idx()] {
                let mut buffer = [0u8; 16];
                let len = device.name(group, index, &mut buffer).min(16);
                let name = core::str::from_utf8(&buffer[..len]).unwrap_or("");
                app.names[group.idx()][index] = if name.is_empty() {
                    let mut text = Text::new();
                    let _ = write!(text, "{} {}", ["In", "Play", "Out"][group.idx()], index + 1);
                    text
                } else {
                    Text::from(name)
                };
            }
        }
        app.rederive(device);
        app
    }

    pub fn outputs(&self) -> Strips {
        self.mix.strips(Group::Output)
    }

    pub fn sources(&self) -> Strips {
        self.mix.sources()
    }

    /// Strips of the Outputs view: the sources, then the master when the device has masters.
    pub fn strip_count(&self) -> usize {
        self.sources().len() + usize::from(self.shape.masters)
    }

    pub fn output(&self) -> Strip {
        let outputs = self.outputs();
        outputs[self.out_sel.min(outputs.len().saturating_sub(1))]
    }

    /// A strip's label: "Play L/R" for a pair named "Play L" and "Play R", "A+B" for other pairs.
    pub fn label(&self, strip: &Strip) -> Text<24> {
        let names = &self.names[strip.group.idx()];
        let first = names[strip.first].as_str();
        let mut text = Text::new();
        if !strip.stereo {
            let _ = text.write_str(first);
            return text;
        }
        let second = names[strip.first + 1].as_str();
        match (first.rsplit_once(' '), second.rsplit_once(' ')) {
            (Some((a, x)), Some((b, y))) if a == b => {
                let _ = write!(text, "{a} {x}/{y}");
            }
            _ => {
                let _ = write!(text, "{first}+{second}");
            }
        }
        text
    }

    pub fn name(&self, group: Group, index: usize) -> &str {
        self.names[group.idx()][index].as_str()
    }

    /// Reads the device again when someone else changed it (its serial moved); true if it did.
    pub fn tick(&mut self, device: &mut impl Device) -> bool {
        if device.serial() == self.seen {
            return false;
        }
        self.rederive(device);
        true
    }

    /// A queued write failed after the app moved on (`queue`): say so and read the device again.
    pub fn refused(&mut self, device: &mut impl Device) {
        self.note(format_args!("the device refused a change"));
        self.rederive(device);
    }

    fn rederive(&mut self, device: &mut impl Device) {
        let masters = self.shape.masters;
        let unity = self.shape.unity;
        self.mix.derive(|o, s| device.gain(o, s), |o| if masters { device.master(o) } else { Some(unity) });
        self.controls = (0..self.shape.controls.min(32)).filter_map(|i| device.control(i)).collect();
        self.seen = device.serial();
    }

    fn note(&mut self, args: core::fmt::Arguments) {
        self.message.clear();
        let _ = self.message.write_fmt(args);
    }

    fn target(&self) -> Option<Target> {
        let output = self.output();
        let sources = self.sources();
        match self.view {
            View::Outputs => match sources.get(self.src_sel) {
                Some(&source) => Some(Target::Send(output, source)),
                None if self.shape.masters => Some(Target::Master(output)),
                None => None,
            },
            View::Matrix => {
                let outputs = self.outputs();
                Some(Target::Send(*outputs.get(self.col)?, *sources.get(self.row)?))
            }
            View::Device => None,
        }
    }

    fn write_cells(&mut self, device: &mut impl Device, cells: &[Cell]) {
        for &(o, s, raw) in cells {
            if device.gain(o, s) != Some(raw) && !device.set_gain(o, s, raw) {
                self.note(format_args!("the device refused a gain (out {} source {})", o + 1, s + 1));
                self.rederive(device);
                return;
            }
        }
        self.seen = device.serial();
    }

    fn write_masters(&mut self, device: &mut impl Device, cells: &[(usize, u32)]) {
        for &(o, raw) in cells {
            if device.master(o) != Some(raw) && !device.set_master(o, raw) {
                self.note(format_args!("the device refused master {}", o + 1));
                self.rederive(device);
                return;
            }
        }
        self.seen = device.serial();
    }

    /// Writes the whole logical state (after the pan law changed).
    fn write_all(&mut self, device: &mut impl Device) {
        let mut failed = false;
        let masters = self.shape.masters;
        self.mix.render(|o, s, raw| {
            if failed {
                return;
            }
            if s == mixer_model::MASTER_SOURCE {
                if masters && device.master(o) != Some(raw) {
                    failed = !device.set_master(o, raw);
                }
            } else if device.gain(o, s) != Some(raw) {
                failed = !device.set_gain(o, s, raw);
            }
        });
        if failed {
            self.note(format_args!("the device refused a gain"));
            self.rederive(device);
        }
        self.seen = device.serial();
    }

    /// Changes the selected send or master.
    fn edit(&mut self, device: &mut impl Device, change: impl Fn(&mut Send) -> bool, master_change: impl Fn(&mut Master) -> bool) {
        match self.target() {
            Some(Target::Send(o, s)) => {
                let mut send = self.mix.send(&o, &s);
                if !change(&mut send) {
                    return;
                }
                let cells = self.mix.set_send(&o, &s, send);
                self.write_cells(device, &cells);
            }
            Some(Target::Master(o)) => {
                let mut master = self.mix.master(&o);
                if !master_change(&mut master) {
                    return;
                }
                let cells = self.mix.set_master(&o, master);
                self.write_masters(device, &cells);
            }
            None => {}
        }
    }

    pub fn act(&mut self, device: &mut impl Device, action: Action) {
        if let Some((index, value)) = self.confirm {
            match action {
                Action::Yes => {
                    self.confirm = None;
                    self.set_control(device, index, value);
                }
                Action::No => self.confirm = None,
                Action::Quit => self.quit = true,
                _ => {}
            }
            return;
        }
        match action {
            Action::Quit => self.quit = true,
            Action::Page(page) => {
                self.page = page;
                self.message.clear();
            }
            Action::View(view) => {
                self.page = Page::Mixer;
                self.view = view;
                self.message.clear();
            }
            Action::AnView(view) => {
                self.page = Page::Analyzer;
                self.analysis.view = view;
                self.message.clear();
            }
            Action::NextView => match self.page {
                Page::Mixer => {
                    self.view = match self.view {
                        View::Outputs => View::Matrix,
                        View::Matrix => View::Device,
                        View::Device => View::Outputs,
                    }
                }
                Page::Analyzer => self.analysis.next_view(),
                Page::Meters => self.metering.next_scale(),
                Page::Recorder => {}
            },
            _ if self.page == Page::Analyzer => self.act_analyzer(action),
            _ if self.page == Page::Meters => self.act_meters(device, action),
            _ if self.page != Page::Mixer => {}
            Action::PanLaw => {
                self.mix.pan_law = match self.mix.pan_law {
                    PanLaw::ConstantPower => PanLaw::Balance,
                    PanLaw::Balance => PanLaw::ConstantPower,
                };
                self.write_all(device);
                self.note(format_args!("pan law {}", self.mix.pan_law.name()));
            }
            Action::Pair => self.pair(device, false),
            Action::PairOutput => self.pair(device, true),
            _ => match self.view {
                View::Outputs => self.act_outputs(device, action),
                View::Matrix => self.act_matrix(device, action),
                View::Device => self.act_device(device, action),
            },
        }
    }

    fn act_analyzer(&mut self, action: Action) {
        let a = &mut self.analysis;
        match action {
            Action::Left => a.step_pair(false),
            Action::Right => a.step_pair(true),
            Action::Pan(step) => a.step_pair(step > 0.0),
            Action::Up => a.step_span(true),
            Action::Down => a.step_span(false),
            Action::Freeze => a.frozen = !a.frozen,
            Action::WaveMode => {
                a.wave = match a.wave {
                    WaveMode::Envelope => WaveMode::Scope,
                    WaveMode::Scope => WaveMode::Envelope,
                };
                a.restart();
            }
            Action::Reset => a.restart(),
            Action::WaveScale => a.db = !a.db,
            Action::Resolution => a.next_resolution(),
            _ => {}
        }
    }

    fn act_meters(&mut self, device: &mut impl Device, action: Action) {
        let m = &mut self.metering;
        match action {
            Action::Left => m.step(false),
            Action::Right => m.step(true),
            Action::Home => m.input = 0,
            Action::End => m.input = m.channels.max(1) - 1,
            Action::MeterScale => m.next_scale(),
            Action::Reset => m.reset(),
            Action::Up => self.step_gain(device, 1),
            Action::Down => self.step_gain(device, -1),
            Action::Step(d) => self.step_gain(device, if d.abs() >= 1.0 { d as i64 } else if d < 0.0 { -1 } else { 1 }),
            Action::Phantom => self.input_switch(device, true),
            Action::InputSwitch => self.input_switch(device, false),
            _ => {}
        }
    }

    /// The device controls of capture input `input` (0-based), in the device's order (`meters::input_of`).
    pub fn input_controls(&self, input: usize) -> List<usize, 4> {
        let mut list = List::new();
        for (index, control) in self.controls.iter().enumerate() {
            if meters::input_of(field(&control.name)) == Some(input + 1) && list.len() < 4 {
                list.push(index);
            }
        }
        list
    }

    /// Rows of control cells under the meters: the most any input has.
    pub fn meter_rows(&self) -> usize {
        (0..self.metering.channels.min(MAX_CHANNELS)).map(|input| self.input_controls(input).len()).max().unwrap_or(0)
    }

    /// The control Up and Down change for `input`: its first number (a gain), else its first choice (a level switch).
    pub fn gain_control(&self, input: usize) -> Option<usize> {
        let controls = self.input_controls(input);
        let of_kind = |kind| controls.iter().copied().find(|&i| self.controls[i].kind == kind);
        of_kind(fosfor_abi::tape::MIXER_CONTROL_INT).or_else(|| of_kind(MIXER_CONTROL_ENUM))
    }

    fn step_gain(&mut self, device: &mut impl Device, delta: i64) {
        let input = self.metering.input;
        match self.gain_control(input) {
            Some(index) => self.change_control(device, index, delta),
            None => self.note(format_args!("input {} has no gain here", input + 1)),
        }
    }

    /// Flips the selected input's phantom power (`phantom`) or its first other switch (instrument, pad); a hazardous one
    /// goes through `change_control`, which asks when `ask_hazard` is on.
    fn input_switch(&mut self, device: &mut impl Device, phantom: bool) {
        let input = self.metering.input;
        let found = self.input_controls(input).iter().copied().find(|&i| {
            let control = &self.controls[i];
            let is_phantom = field(&control.name).contains("48V");
            control.kind == MIXER_CONTROL_BOOL && is_phantom == phantom
        });
        match found {
            Some(index) => self.change_control(device, index, 1),
            None => self.note(format_args!("input {} has no {} switch", input + 1, if phantom { "+48V" } else { "other" })),
        }
    }

    /// A control's value in a meter's cell, a few characters: a switch by its name (the drawing lights it when on), a
    /// number with its unit ("40dB"), a choice by its item; "?" while the device does not know it.
    pub fn control_short(&self, device: &impl Device, index: usize) -> Text<12> {
        let mut text = Text::new();
        let Some(control) = self.controls.get(index) else { return text };
        let label = meters::short_label(field(&control.name));
        let unknown = control.flags & MIXER_FLAG_UNKNOWN != 0;
        match control.kind {
            MIXER_CONTROL_BOOL => {
                let _ = write!(text, "{label}{}", if unknown { "?" } else { "" });
            }
            _ if unknown => {
                let _ = text.write_str("?");
            }
            MIXER_CONTROL_ENUM => {
                let mut buffer = [0u8; 12];
                let len = device.control_item(index, control.value as usize, &mut buffer).min(12);
                match core::str::from_utf8(&buffer[..len]) {
                    Ok(item) if !item.is_empty() => {
                        let _ = text.write_str(item);
                    }
                    _ => {
                        let _ = write!(text, "{}", control.value);
                    }
                }
            }
            _ => {
                let _ = write!(text, "{}{}", control.value, field(&control.unit));
            }
        }
        text
    }

    /// The soft keys for the page shown (`SOFT_SLOTS` of them, some `Empty`).
    pub fn soft_keys(&self) -> [Soft; SOFT_SLOTS] {
        let mut keys = [Soft::EMPTY; SOFT_SLOTS];
        let mut slot = 0;
        let mut put = |slot: &mut usize, label, kind, action, active| {
            keys[*slot] = Soft { label, kind, action, active };
            *slot += 1;
        };
        let (mixer, analyzer) = (self.page == Page::Mixer, self.page == Page::Analyzer);
        put(&mut slot, "Mixer", SoftKind::Page, Action::Page(Page::Mixer), mixer);
        if mixer {
            for (label, view) in [("Outputs", View::Outputs), ("Matrix", View::Matrix), ("Device", View::Device)] {
                put(&mut slot, label, SoftKind::View, Action::View(view), self.view == view);
            }
        }
        put(&mut slot, "Recorder", SoftKind::Page, Action::Page(Page::Recorder), self.page == Page::Recorder);
        put(&mut slot, "Analyzer", SoftKind::Page, Action::Page(Page::Analyzer), analyzer);
        if analyzer {
            for (label, view) in [("Waveform", AnView::Waveform), ("Spectrum", AnView::Spectrum), ("Both", AnView::Both)] {
                put(&mut slot, label, SoftKind::View, Action::AnView(view), self.analysis.view == view);
            }
        }
        put(&mut slot, "Meters", SoftKind::Page, Action::Page(Page::Meters), self.page == Page::Meters);
        slot = SOFT_NAV;
        if mixer {
            put(&mut slot, "Pair", SoftKind::Action, Action::Pair, false);
            put(&mut slot, "Pan law", SoftKind::Action, Action::PanLaw, false);
        } else if analyzer {
            put(&mut slot, "Freeze", SoftKind::Action, Action::Freeze, self.analysis.frozen);
            let wave = if self.analysis.wave == WaveMode::Envelope { "Wave: Env" } else { "Wave: Scope" };
            put(&mut slot, wave, SoftKind::Action, Action::WaveMode, false);
            put(&mut slot, "Reset", SoftKind::Action, Action::Reset, false);
        } else if self.page == Page::Meters {
            let scale = ["Scale: dBFS", "Scale: PPM", "Scale: LUFS"][self.metering.scale as usize];
            put(&mut slot, scale, SoftKind::Action, Action::MeterScale, false);
            put(&mut slot, "Reset", SoftKind::Action, Action::Reset, false);
        }
        slot = SOFT_NAV + SOFT_ACTIONS;
        put(&mut slot, "Quit", SoftKind::Action, Action::Quit, false);
        keys
    }

    /// Pairing only changes how the device's crosspoints are shown, so the device is read again and nothing is written.
    fn pair(&mut self, device: &mut impl Device, output: bool) {
        let strip = match (self.view, output) {
            (View::Matrix, false) => self.sources().get(self.row).copied(),
            (View::Matrix, true) => self.outputs().get(self.col).copied(),
            (_, false) => self.sources().get(self.src_sel).copied().or(Some(self.output())),
            (_, true) => Some(self.output()),
        };
        let Some(strip) = strip else { return };
        if self.mix.toggle_pair(strip.group, strip.first) {
            self.rederive(device);
            self.clamp();
            let mut changed = strip;
            changed.first &= !1;
            let label = self.label(&Strip { stereo: false, ..changed });
            self.note(format_args!("{} {}", label.as_str(), if self.mix.paired[strip.group.idx()][changed.first] { "paired" } else { "split" }));
        } else {
            self.note(format_args!("that channel has no partner"));
        }
    }

    fn clamp(&mut self) {
        let outputs = self.outputs().len();
        let sources = self.sources().len();
        self.out_sel = self.out_sel.min(outputs.saturating_sub(1));
        self.src_sel = self.src_sel.min(self.strip_count().saturating_sub(1));
        self.row = self.row.min(sources.saturating_sub(1));
        self.col = self.col.min(outputs.saturating_sub(1));
    }

    /// The edit keys shared by the Outputs and Matrix views; false if `action` is not one.
    fn act_edit(&mut self, device: &mut impl Device, action: Action) -> bool {
        match action {
            Action::Step(delta) => self.edit(
                device,
                |send| {
                    send.db = mixer_model::step_db(send.db, delta);
                    true
                },
                |master| {
                    master.db = mixer_model::step_db(master.db, delta);
                    true
                },
            ),
            Action::Unity => self.edit(
                device,
                |send| {
                    send.db = 0.0;
                    true
                },
                |master| {
                    master.db = 0.0;
                    true
                },
            ),
            Action::Off => self.edit(
                device,
                |send| {
                    *send = Send { pan: send.pan, ..Send::OFF };
                    true
                },
                |master| {
                    *master = Master::OFF;
                    true
                },
            ),
            Action::Mute => self.edit(
                device,
                |send| {
                    send.mute = !send.mute;
                    true
                },
                |master| {
                    master.mute = !master.mute;
                    true
                },
            ),
            Action::Pan(delta) => self.edit(
                device,
                |send| {
                    send.pan = round_pan(send.pan + delta);
                    true
                },
                |_| false,
            ),
            Action::Center => self.edit(
                device,
                |send| {
                    send.pan = 0.0;
                    true
                },
                |_| false,
            ),
            _ => return false,
        }
        true
    }

    fn act_outputs(&mut self, device: &mut impl Device, action: Action) {
        if self.act_edit(device, action) {
            return;
        }
        let outputs = self.outputs().len();
        let last = self.strip_count().saturating_sub(1);
        match action {
            Action::Up => self.act_edit(device, Action::Step(1.0)),
            Action::Down => self.act_edit(device, Action::Step(-1.0)),
            Action::Left => {
                self.src_sel = self.src_sel.saturating_sub(1);
                true
            }
            Action::Right => {
                self.src_sel = (self.src_sel + 1).min(last);
                true
            }
            Action::Home => {
                self.src_sel = 0;
                true
            }
            Action::End => {
                self.src_sel = last;
                true
            }
            Action::NextOutput => {
                self.out_sel = (self.out_sel + 1) % outputs.max(1);
                true
            }
            Action::PrevOutput => {
                self.out_sel = (self.out_sel + outputs.max(1) - 1) % outputs.max(1);
                true
            }
            _ => false,
        };
    }

    fn act_matrix(&mut self, device: &mut impl Device, action: Action) {
        if self.act_edit(device, action) {
            return;
        }
        let rows = self.sources().len();
        let cols = self.outputs().len();
        match action {
            Action::Up => self.row = self.row.saturating_sub(1),
            Action::Down => self.row = (self.row + 1).min(rows.saturating_sub(1)),
            Action::Left => self.col = self.col.saturating_sub(1),
            Action::Right => self.col = (self.col + 1).min(cols.saturating_sub(1)),
            Action::Home => self.row = 0,
            Action::End => self.row = rows.saturating_sub(1),
            Action::Enter => self.open_cell(),
            _ => {}
        }
    }

    /// From the Matrix view: the selected send in the Outputs view.
    fn open_cell(&mut self) {
        self.out_sel = self.col;
        self.src_sel = self.row;
        self.view = View::Outputs;
    }

    /// Rows of the Device page: the device's controls, then the app's own "Ask before +48V" switch.
    pub fn device_rows(&self) -> usize {
        self.controls.len() + 1
    }

    /// The Device page row of the "Ask before +48V" switch.
    pub fn ask_row(&self) -> usize {
        self.controls.len()
    }

    fn act_device(&mut self, device: &mut impl Device, action: Action) {
        let count = self.device_rows();
        let delta: i64 = match action {
            Action::Up => {
                self.ctl_sel = self.ctl_sel.saturating_sub(1);
                return;
            }
            Action::Down => {
                self.ctl_sel = (self.ctl_sel + 1).min(count - 1);
                return;
            }
            Action::Step(d) if d < 0.0 => -1,
            Action::Left => -1,
            Action::Right | Action::Step(_) | Action::Enter => 1,
            _ => return,
        };
        self.change_control(device, self.ctl_sel, delta);
    }

    /// Steps control `index` by `delta` (a bool or an action just toggles or fires); a hazardous change asks first.
    fn change_control(&mut self, device: &mut impl Device, index: usize, delta: i64) {
        if index == self.ask_row() {
            self.ask_hazard = !self.ask_hazard;
            self.ask_changed = true;
            return;
        }
        let Some(&control) = self.controls.get(index) else { return };
        let name = field(&control.name);
        if control.flags & MIXER_FLAG_READONLY != 0 {
            self.note(format_args!("{name} is read-only"));
            return;
        }
        let value = match control.kind {
            MIXER_CONTROL_BOOL => 1 - control.value.clamp(0, 1),
            MIXER_CONTROL_ACTION => 1,
            MIXER_CONTROL_ENUM => {
                let span = control.max - control.min + 1;
                control.min + (control.value - control.min + delta.signum()).rem_euclid(span.max(1))
            }
            _ => (control.value + delta).clamp(control.min, control.max),
        };
        if value == control.value && control.kind != MIXER_CONTROL_ACTION {
            return;
        }
        if self.asks(&control) && (value != 0 || control.kind == MIXER_CONTROL_ACTION) {
            self.confirm = Some((index, value));
        } else {
            self.set_control(device, index, value);
        }
    }

    /// Whether a change to `control` waits for a yes: a hazardous action always, a hazardous switch when `ask_hazard` is on.
    pub fn asks(&self, control: &MixerControl) -> bool {
        control.flags & MIXER_FLAG_HAZARD != 0 && (control.kind == MIXER_CONTROL_ACTION || self.ask_hazard)
    }

    fn set_control(&mut self, device: &mut impl Device, index: usize, value: i64) {
        if !device.set_control(index, value) {
            let name = self.controls.get(index).map(|c| c.name).unwrap_or_default();
            self.note(format_args!("the device refused {}", field(&name)));
        }
        self.rederive(device);
    }

    /// A control's value as text ("On", an enum item, "48000 Hz", "run"; "?" while the device does not know it).
    pub fn control_value(&self, device: &impl Device, index: usize) -> Text<32> {
        let mut text = Text::new();
        let Some(control) = self.controls.get(index) else { return text };
        if control.flags & MIXER_FLAG_UNKNOWN != 0 && control.kind != MIXER_CONTROL_ACTION {
            let _ = text.write_str("?");
            return text;
        }
        match control.kind {
            MIXER_CONTROL_BOOL => {
                let _ = text.write_str(if control.value != 0 { "On" } else { "Off" });
            }
            MIXER_CONTROL_ACTION => {
                let _ = text.write_str("run");
            }
            MIXER_CONTROL_ENUM => {
                let mut buffer = [0u8; 32];
                let len = device.control_item(index, control.value as usize, &mut buffer).min(32);
                let item = core::str::from_utf8(&buffer[..len]).unwrap_or("");
                if item.is_empty() {
                    let _ = write!(text, "{}", control.value);
                } else {
                    let _ = text.write_str(item);
                }
            }
            _ => {
                let unit = field(&control.unit);
                let _ = write!(text, "{}{}{}", control.value, if unit.is_empty() { "" } else { " " }, unit);
            }
        }
        text
    }

    /// The send or master strip `index` of the Outputs view shows (sources, then the master).
    pub fn strip_level(&self, index: usize) -> (f64, f64, bool, bool, bool) {
        let output = self.output();
        match self.sources().get(index) {
            Some(source) => {
                let send = self.mix.send(&output, source);
                (send.db, send.pan, send.mute, send.unknown, true)
            }
            None => {
                let master = self.mix.master(&output);
                (master.db, 0.0, master.mute, master.unknown, false)
            }
        }
    }

    /// Keeps the selections inside the window each list has in `layout`.
    pub fn scroll(&mut self, layout: &Layout) {
        self.clamp();
        self.first_strip = keep(self.src_sel, self.first_strip, layout.visible_strips(), self.strip_count());
        self.first_tab = keep(self.out_sel, self.first_tab, layout.visible_tabs(), self.outputs().len());
        self.first_row = keep(self.row, self.first_row, layout.visible_rows(), self.sources().len());
        self.first_col = keep(self.col, self.first_col, layout.visible_cols(), self.outputs().len());
        self.first_ctl = keep(self.ctl_sel, self.first_ctl, layout.visible_controls(), self.device_rows());
        self.metering.input = self.metering.input.min(self.metering.channels.max(1) - 1);
        self.first_meter = keep(self.metering.bar(), self.first_meter, layout.visible_meters(self), self.metering.bars());
    }

    /// A mouse button or finger came down at (`x`, `y`); `Some` when it grabbed a fader or a pan control to drag.
    pub fn press(&mut self, device: &mut impl Device, layout: &Layout, x: i32, y: i32) -> Option<Grab> {
        let hit = layout.hit(self, x, y);
        if self.confirm.is_some() {
            match hit {
                Hit::Yes => self.act(device, Action::Yes),
                Hit::No => self.act(device, Action::No),
                _ => {}
            }
            return None;
        }
        match hit {
            Hit::Soft(slot) => {
                let key = self.soft_keys()[slot];
                if key.kind != SoftKind::Empty {
                    self.act(device, key.action);
                }
                None
            }
            Hit::Tab(output) => {
                self.out_sel = output;
                None
            }
            Hit::Label(strip) => {
                self.src_sel = strip;
                None
            }
            Hit::Fader(strip) => {
                self.src_sel = strip;
                self.drag(device, layout, Grab::Fader(strip), x, y);
                Some(Grab::Fader(strip))
            }
            Hit::Pan(strip) => {
                self.src_sel = strip;
                self.drag(device, layout, Grab::Pan(strip), x, y);
                Some(Grab::Pan(strip))
            }
            Hit::Cell(row, col) => {
                if (row, col) == (self.row, self.col) {
                    self.open_cell();
                } else {
                    self.row = row;
                    self.col = col;
                }
                None
            }
            Hit::Control(index) => {
                if index == self.ctl_sel {
                    self.change_control(device, index, 1);
                } else {
                    self.ctl_sel = index;
                }
                None
            }
            Hit::Meter(input) => {
                self.metering.input = input;
                None
            }
            Hit::InputControl(index, input) => {
                self.metering.input = input;
                let control = self.controls.get(index)?;
                if control.kind == fosfor_abi::tape::MIXER_CONTROL_INT {
                    return Some(Grab::Control { index, y, value: control.value });
                }
                self.change_control(device, index, 1);
                None
            }
            Hit::Yes | Hit::No | Hit::Nothing => None,
        }
    }

    /// The held button or finger moved to (`x`, `y`): the grabbed fader follows it (0.5 dB steps, the bottom is off), a
    /// grabbed pan control follows it sideways (1% steps), a grabbed gain under a meter one step per half text row.
    pub fn drag(&mut self, device: &mut impl Device, layout: &Layout, grab: Grab, x: i32, y: i32) {
        if let Grab::Control { index, y: from, value } = grab {
            let Some(control) = self.controls.get(index).filter(|_| self.page == Page::Meters && self.confirm.is_none()) else { return };
            let target = (value + ((from - y) / (layout.row / 2).max(1)) as i64).clamp(control.min, control.max);
            if target != control.value {
                self.change_control(device, index, target - control.value);
            }
            return;
        }
        if self.view != View::Outputs || self.page != Page::Mixer {
            return;
        }
        match grab {
            Grab::Control { .. } => {}
            Grab::Fader(strip) => {
                let Some(fader) = layout.fader(strip, self) else { return };
                let position = (fader.y + fader.h - y) as f64 / fader.h.max(1) as f64;
                let db = layout::fader_db(position);
                self.src_sel = strip;
                self.edit(
                    device,
                    |send| {
                        let changed = send.db != db || send.unknown;
                        send.db = db;
                        changed
                    },
                    |master| {
                        let changed = master.db != db || master.unknown;
                        master.db = db;
                        changed
                    },
                );
            }
            Grab::Pan(strip) => {
                let Some(pan) = layout.pan(strip, self) else { return };
                let value = round_pan(((x - pan.x) as f64 / pan.w.max(1) as f64) * 2.0 - 1.0);
                self.src_sel = strip;
                self.edit(
                    device,
                    |send| {
                        let changed = send.pan != value;
                        send.pan = value;
                        changed
                    },
                    |_| false,
                );
            }
        }
    }
}

fn round_pan(pan: f64) -> f64 {
    let hundredths = pan.clamp(-1.0, 1.0) * 100.0;
    let rounded = if hundredths < 0.0 { -((-hundredths + 0.5) as i64) } else { (hundredths + 0.5) as i64 };
    rounded as f64 / 100.0
}

/// The first item to show so that `selected` stays in a window of `visible` out of `count`, moving `first` only when it must.
pub fn keep(selected: usize, first: usize, visible: usize, count: usize) -> usize {
    let visible = visible.max(1);
    let first = if selected < first { selected } else if selected >= first + visible { selected + 1 - visible } else { first };
    first.min(count.saturating_sub(visible))
}

/// A send or master level as text: "?" unknown, "mute", "off", else "+1.5".
pub fn format_db(db: f64, mute: bool, unknown: bool) -> Text<8> {
    let mut text = Text::new();
    if unknown {
        let _ = text.write_str("?");
    } else if mute {
        let _ = text.write_str("mute");
    } else if !db.is_finite() {
        let _ = text.write_str("off");
    } else {
        let tenths = (db * 10.0 + if db < 0.0 { -0.5 } else { 0.5 }) as i64;
        let sign = if tenths < 0 { "-" } else { "+" };
        let _ = write!(text, "{sign}{}.{}", tenths.abs() / 10, tenths.abs() % 10);
    }
    text
}

/// A pan position as text: "C", "L50", "R25".
pub fn format_pan(pan: f64) -> Text<8> {
    let mut text = Text::new();
    let percent = (pan.abs() * 100.0 + 0.5) as i64;
    if percent == 0 {
        let _ = text.write_str("C");
    } else {
        let _ = write!(text, "{}{percent}", if pan < 0.0 { "L" } else { "R" });
    }
    text
}

#[cfg(test)]
mod tests;
