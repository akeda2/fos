//! The phono equaliser tape without its audio and screen: the settings (`Setup`: read from `key=value` words, written back for
//! `save`, applied to a `phono::Phono`) and the touch screen's geometry and meaning (`Layout`, `Hit`, `press`, `drag`).
//! `main.rs` runs it, as a headless service-style tape or with the picture; the logic is testable on the host.
#![cfg_attr(not(test), no_std)]

use core::fmt::Write;
use instrument_rt::Rect;
use phono::{BassBoost, CURVES, Polarity, Phono, Rumble, curves};
use tape_rt::Line;

pub const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDGETS: usize = 40;
pub const SLIDERS: [&str; 5] = ["Bass boost", "Bass freq", "Bass Q", "Rumble", "Trim"];
pub const F_MIN: f32 = 20.0;
pub const F_MAX: f32 = 20000.0;
pub const DB_MIN: f32 = -30.0;
pub const DB_MAX: f32 = 30.0;
const RUMBLE_MIN_HZ: f32 = 10.0;
/// The first few hundredths of the Rumble slider are "off".
const RUMBLE_OFF_BELOW: f32 = 0.03;
const TRIM_MIN_DB: f32 = -24.0;
const TRIM_MAX_DB: f32 = 12.0;
/// Half the width (as a fraction of the slider) of the flat stretch where the trim is exactly 0 dB; the dB scale continues
/// either side of it, so every value stays reachable and a finger can still hit 0.
const TRIM_ZERO_HALF: f32 = 0.025;
pub const DEFAULT_FILE: &str = "phono.cfg";
pub const TAPE_NAME: &str = "phono";

/// Everything the tape remembers; `Default` is RIAA, stereo, normal polarity, no rumble filter, no boost.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Setup {
    pub curve: usize,
    pub mono: bool,
    pub polarity: Polarity,
    /// 0 is off.
    pub rumble_hz: f32,
    pub bass: BassBoost,
    pub trim_db: f32,
}

impl Default for Setup {
    fn default() -> Self {
        Setup { curve: curves::RIAA, mono: false, polarity: Polarity::Normal, rumble_hz: 0.0, bass: BassBoost::OFF, trim_db: 0.0 }
    }
}

fn parse_switch(value: &str) -> Option<bool> {
    match value {
        "on" | "yes" | "true" | "1" => Some(true),
        "off" | "no" | "false" | "0" => Some(false),
        _ => None,
    }
}

fn number(key: &str, value: &str, min: f32, max: f32) -> Result<f32, Line<96>> {
    match tape_rt::parse_f32(value) {
        Some(v) if v >= min && v <= max => Ok(v),
        _ => {
            let mut error = Line::<96>::new();
            let _ = write!(error, "{key}= needs a number from ");
            tenths(&mut error, min);
            let _ = write!(error, " to ");
            tenths(&mut error, max);
            Err(error)
        }
    }
}

fn bad(key: &str, expected: &str) -> Line<96> {
    let mut error = Line::<96>::new();
    let _ = write!(error, "{key}= takes {expected}");
    error
}

/// `value` with at most one decimal, e.g. `-3.5`, `12`, `0.7`: short enough for a settings line and a button.
pub fn tenths(out: &mut impl Write, value: f32) {
    let scaled = if value < 0.0 { (value * 10.0 - 0.5) as i32 } else { (value * 10.0 + 0.5) as i32 };
    let (sign, scaled) = if scaled < 0 { ("-", -scaled) } else { ("", scaled) };
    if scaled % 10 == 0 {
        let _ = write!(out, "{sign}{}", scaled / 10);
    } else {
        let _ = write!(out, "{sign}{}.{}", scaled / 10, scaled % 10);
    }
}

impl Setup {
    /// Takes the words `curve=ID mono=on|off polarity=normal|left|right|both rumble=off|HZ bass=DB bassfreq=HZ bassq=Q
    /// trim=DB` from `text` (any may be missing). The first bad value is the error.
    pub fn read(&mut self, text: &str) -> Result<(), Line<96>> {
        if let Some(value) = tape_rt::arg_value(text, "curve") {
            let Some(index) = curves::find(value) else {
                let mut error = Line::<96>::new();
                let _ = write!(error, "no curve {value} (`phono help` lists them)");
                return Err(error);
            };
            self.curve = index;
        }
        if let Some(value) = tape_rt::arg_value(text, "mono") {
            self.mono = parse_switch(value).ok_or_else(|| bad("mono", "on or off"))?;
        }
        if let Some(value) = tape_rt::arg_value(text, "polarity") {
            self.polarity = Polarity::parse(value).ok_or_else(|| bad("polarity", "normal, left, right or both"))?;
        }
        if let Some(value) = tape_rt::arg_value(text, "rumble") {
            self.rumble_hz = if parse_switch(value) == Some(false) { 0.0 } else { number("rumble", value, RUMBLE_MIN_HZ, 100.0)? };
        }
        if let Some(value) = tape_rt::arg_value(text, "bass") {
            self.bass.gain_db = number("bass", value, 0.0, 12.0)?;
        }
        if let Some(value) = tape_rt::arg_value(text, "bassfreq") {
            self.bass.hz = number("bassfreq", value, 60.0, 150.0)?;
        }
        if let Some(value) = tape_rt::arg_value(text, "bassq") {
            self.bass.q = number("bassq", value, 0.3, 3.0)?;
        }
        if let Some(value) = tape_rt::arg_value(text, "trim") {
            self.trim_db = number("trim", value, TRIM_MIN_DB, TRIM_MAX_DB)?;
        }
        Ok(())
    }

    /// The `key=value` lines `read` takes (the file the service reads).
    pub fn write<const N: usize>(&self, out: &mut tape_rt::config::Settings<N>) {
        out.put("curve", format_args!("{}", CURVES[self.curve].id));
        out.put("mono", format_args!("{}", if self.mono { "on" } else { "off" }));
        out.put("polarity", format_args!("{}", self.polarity.key()));
        if self.rumble_hz > 0.0 {
            let mut hz = Line::<16>::new();
            tenths(&mut hz, self.rumble_hz);
            out.put("rumble", format_args!("{}", hz.as_str()));
        } else {
            out.put("rumble", format_args!("off"));
        }
        for (key, value) in [("bass", self.bass.gain_db), ("bassfreq", self.bass.hz), ("bassq", self.bass.q), ("trim", self.trim_db)] {
            let mut text = Line::<16>::new();
            tenths(&mut text, value);
            out.put(key, format_args!("{}", text.as_str()));
        }
    }

    pub fn apply(&self, eq: &mut Phono) {
        eq.set_curve(self.curve);
        eq.set_mono(self.mono);
        eq.set_polarity(self.polarity);
        eq.set_rumble(if self.rumble_hz > 0.0 { Rumble::Steep(self.rumble_hz) } else { Rumble::Off });
        eq.set_bass_boost(self.bass);
        eq.set_trim_db(self.trim_db);
    }

    pub fn invert_left(&self) -> bool {
        matches!(self.polarity, Polarity::InvertLeft | Polarity::InvertBoth)
    }

    pub fn invert_right(&self) -> bool {
        matches!(self.polarity, Polarity::InvertRight | Polarity::InvertBoth)
    }

    pub fn set_inversion(&mut self, left: bool, right: bool) {
        self.polarity = match (left, right) {
            (false, false) => Polarity::Normal,
            (true, false) => Polarity::InvertLeft,
            (false, true) => Polarity::InvertRight,
            (true, true) => Polarity::InvertBoth,
        };
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Index into `CURVES`.
    Curve(u8),
    /// Index into `SLIDERS`.
    Slider(u8),
    Mono,
    InvertLeft,
    InvertRight,
    Save,
}

#[derive(Clone, Copy)]
pub struct Widget {
    pub rect: Rect,
    pub hit: Hit,
}

pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub margin: i32,
    pub header: Rect,
    pub plot: Rect,
    widgets: [Widget; MAX_WIDGETS],
    widget_count: usize,
}

impl Layout {
    /// The response plot top left, the curve buttons top right (3 columns), five sliders bottom left, the switches and Save
    /// bottom right.
    pub fn new(width: i32, height: i32) -> Self {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let gap = margin / 2;
        let header_h = GLYPH_H * scale * 2 + margin * 2;
        let top = header_h + margin;
        let body_h = height - top - margin;
        let upper_h = body_h * 48 / 100;
        let lower_y = top + upper_h + margin;
        let lower_h = body_h - upper_h - margin;
        let plot = Rect { x: margin, y: top, w: (width - 3 * margin) * 60 / 100, h: upper_h };
        let mut layout = Layout {
            width,
            height,
            scale,
            margin,
            header: Rect { x: 0, y: 0, w: width, h: header_h },
            plot,
            widgets: [Widget { rect: Rect::ZERO, hit: Hit::Mono }; MAX_WIDGETS],
            widget_count: 0,
        };
        let grid_x = plot.x + plot.w + margin;
        let grid_w = width - grid_x - margin;
        let (columns, rows) = (3, 7);
        let (cell_w, cell_h) = ((grid_w - (columns - 1) * gap) / columns, (upper_h - (rows - 1) * gap) / rows);
        for index in 0..CURVES.len() as i32 {
            let (column, row) = (index % columns, index / columns);
            layout.add(Rect { x: grid_x + column * (cell_w + gap), y: top + row * (cell_h + gap), w: cell_w, h: cell_h }, Hit::Curve(index as u8));
        }
        let sliders_w = (width - 3 * margin) * 62 / 100;
        let row_h = lower_h / SLIDERS.len() as i32;
        for index in 0..SLIDERS.len() as i32 {
            layout.add(Rect { x: margin, y: lower_y + index * row_h, w: sliders_w, h: row_h - gap }, Hit::Slider(index as u8));
        }
        let switches_x = margin + sliders_w + margin;
        let switches_w = width - switches_x - margin;
        let (cell_w, cell_h) = ((switches_w - gap) / 2, (lower_h - gap) / 2);
        for (index, hit) in [Hit::Mono, Hit::InvertLeft, Hit::InvertRight, Hit::Save].into_iter().enumerate() {
            let (column, row) = (index as i32 % 2, index as i32 / 2);
            layout.add(Rect { x: switches_x + column * (cell_w + gap), y: lower_y + row * (cell_h + gap), w: cell_w, h: cell_h }, hit);
        }
        layout
    }

    fn add(&mut self, rect: Rect, hit: Hit) {
        self.widgets[self.widget_count] = Widget { rect, hit };
        self.widget_count += 1;
    }

    pub fn widgets(&self) -> &[Widget] {
        &self.widgets[..self.widget_count]
    }

    pub fn widget_rect(&self, hit: Hit) -> Option<Rect> {
        self.widgets().iter().find(|widget| widget.hit == hit).map(|widget| widget.rect)
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.widgets().iter().find(|widget| widget.rect.contains(x, y)).map(|widget| widget.hit)
    }

    pub fn slider_value_at(&self, hit: Hit, x: i32) -> f32 {
        let Some(rect) = self.widget_rect(hit) else { return 0.0 };
        ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0)
    }

    /// Plot column of `hz` (log axis, 20 Hz to 20 kHz).
    pub fn plot_x(&self, hz: f32) -> i32 {
        let position = dsp::math::log10(hz.clamp(F_MIN, F_MAX) / F_MIN) / dsp::math::log10(F_MAX / F_MIN);
        self.plot.x + (position * (self.plot.w - 1) as f32) as i32
    }

    /// Plot row of `db` (clipped to the plot).
    pub fn plot_y(&self, db: f32) -> i32 {
        let position = (db.clamp(DB_MIN, DB_MAX) - DB_MIN) / (DB_MAX - DB_MIN);
        self.plot.y + self.plot.h - 1 - (position * (self.plot.h - 1) as f32) as i32
    }
}

/// A slider's position, 0..=1.
pub fn slider_value(setup: &Setup, index: u8) -> f32 {
    match index {
        0 => setup.bass.gain_db / 12.0,
        1 => dsp::math::log10(setup.bass.hz / 60.0) / dsp::math::log10(150.0 / 60.0),
        2 => dsp::math::log10(setup.bass.q / 0.3),
        3 if setup.rumble_hz <= 0.0 => 0.0,
        3 => RUMBLE_OFF_BELOW + (1.0 - RUMBLE_OFF_BELOW) * dsp::math::log10(setup.rumble_hz / RUMBLE_MIN_HZ),
        _ => {
            let (below, span) = trim_zero_zone();
            let offset = setup.trim_db / (TRIM_MAX_DB - TRIM_MIN_DB) * span;
            if setup.trim_db < 0.0 {
                below + offset
            } else if setup.trim_db == 0.0 {
                below + TRIM_ZERO_HALF
            } else {
                below + 2.0 * TRIM_ZERO_HALF + offset
            }
        }
    }
}

/// Where the trim slider's flat 0 dB zone starts, and the slider length left for the dB scale (the zone takes the rest).
fn trim_zero_zone() -> (f32, f32) {
    let span = 1.0 - 2.0 * TRIM_ZERO_HALF;
    (span * -TRIM_MIN_DB / (TRIM_MAX_DB - TRIM_MIN_DB), span)
}

pub fn set_slider(setup: &mut Setup, index: u8, value: f32) {
    let value = value.clamp(0.0, 1.0);
    match index {
        0 => setup.bass.gain_db = if value < 0.02 { 0.0 } else { value * 12.0 },
        1 => setup.bass.hz = 60.0 * dsp::math::pow10(value * dsp::math::log10(150.0 / 60.0)),
        2 => setup.bass.q = 0.3 * dsp::math::pow10(value),
        3 => {
            setup.rumble_hz = if value < RUMBLE_OFF_BELOW {
                0.0
            } else {
                RUMBLE_MIN_HZ * dsp::math::pow10((value - RUMBLE_OFF_BELOW) / (1.0 - RUMBLE_OFF_BELOW))
            };
        }
        _ => {
            let (below, span) = trim_zero_zone();
            let scale = TRIM_MAX_DB - TRIM_MIN_DB;
            setup.trim_db = if value < below {
                (value - below) / span * scale
            } else if value <= below + 2.0 * TRIM_ZERO_HALF {
                0.0
            } else {
                (value - below - 2.0 * TRIM_ZERO_HALF) / span * scale
            };
        }
    }
}

/// The text on a slider.
pub fn slider_text(setup: &Setup, index: u8, out: &mut impl Write) {
    let _ = write!(out, "{} ", SLIDERS[index as usize]);
    match index {
        0 => {
            tenths(out, setup.bass.gain_db);
            let _ = write!(out, " dB");
        }
        1 => {
            let _ = write!(out, "{} Hz", (setup.bass.hz + 0.5) as i32);
        }
        2 => tenths(out, setup.bass.q),
        3 if setup.rumble_hz <= 0.0 => {
            let _ = write!(out, "off");
        }
        3 => {
            let _ = write!(out, "{} Hz", (setup.rumble_hz + 0.5) as i32);
        }
        _ => {
            tenths(out, setup.trim_db);
            let _ = write!(out, " dB");
        }
    }
}

pub fn label(hit: Hit) -> &'static str {
    match hit {
        Hit::Curve(index) => CURVES[index as usize].short,
        Hit::Slider(index) => SLIDERS[index as usize],
        Hit::Mono => "Mono",
        Hit::InvertLeft => "Invert L",
        Hit::InvertRight => "Invert R",
        Hit::Save => "Save",
    }
}

pub fn lit(setup: &Setup, hit: Hit) -> bool {
    match hit {
        Hit::Curve(index) => setup.curve == index as usize,
        Hit::Mono => setup.mono,
        Hit::InvertLeft => setup.invert_left(),
        Hit::InvertRight => setup.invert_right(),
        _ => false,
    }
}

/// A finger lands on `hit` at column `x`. Save is left to the caller (it needs the file system).
pub fn press(layout: &Layout, setup: &mut Setup, hit: Hit, x: i32) {
    match hit {
        Hit::Curve(index) => setup.curve = index as usize,
        Hit::Mono => setup.mono = !setup.mono,
        Hit::InvertLeft => setup.set_inversion(!setup.invert_left(), setup.invert_right()),
        Hit::InvertRight => setup.set_inversion(setup.invert_left(), !setup.invert_right()),
        Hit::Slider(_) => drag(layout, setup, hit, x),
        Hit::Save => {}
    }
}

/// A finger that landed on a slider moves to column `x`.
pub fn drag(layout: &Layout, setup: &mut Setup, hit: Hit, x: i32) {
    if let Hit::Slider(index) = hit {
        set_slider(setup, index, layout.slider_value_at(hit, x));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(i32, i32); 5] = [(640, 480), (1280, 1024), (1366, 768), (1920, 1080), (3312, 1872)];

    #[test]
    fn every_widget_is_on_screen_and_none_overlap() {
        for (w, h) in SIZES {
            let layout = Layout::new(w, h);
            let screen = Rect { x: 0, y: 0, w, h };
            assert_eq!(layout.widgets().len(), CURVES.len() + SLIDERS.len() + 4);
            assert!(layout.plot.w > 100 && layout.plot.h > 60, "{w}x{h}");
            for (i, a) in layout.widgets().iter().enumerate() {
                let r = a.rect;
                assert!(r.w > 40 && r.h > 8, "{w}x{h} {:?} {}x{}", a.hit, r.w, r.h);
                assert!(screen.contains(r.x, r.y) && screen.contains(r.x + r.w - 1, r.y + r.h - 1), "{w}x{h} {:?}", a.hit);
                for b in &layout.widgets()[i + 1..] {
                    let s = b.rect;
                    assert!(r.x + r.w <= s.x || s.x + s.w <= r.x || r.y + r.h <= s.y || s.y + s.h <= r.y, "{w}x{h} {:?} {:?}", a.hit, b.hit);
                }
            }
        }
    }

    #[test]
    fn curve_labels_fit_their_buttons() {
        for (w, h) in SIZES {
            let layout = Layout::new(w, h);
            for widget in layout.widgets() {
                if let Hit::Curve(_) = widget.hit {
                    let need = gfx::text_width(label(widget.hit)) as i32 * layout.scale;
                    assert!(need + 4 <= widget.rect.w, "{w}x{h} {} needs {need}, has {}", label(widget.hit), widget.rect.w);
                }
            }
        }
    }

    #[test]
    fn settings_round_trip_through_text() {
        let setup = Setup {
            curve: curves::find("columbia-lp").unwrap(),
            mono: true,
            polarity: Polarity::InvertRight,
            rumble_hz: 30.0,
            bass: BassBoost { gain_db: 4.5, hz: 90.0, q: 1.2 },
            trim_db: -3.5,
        };
        let mut body = tape_rt::config::Settings::<256>::new();
        setup.write(&mut body);
        assert!(body.as_str().contains("curve=columbia-lp\n") && body.as_str().contains("trim=-3.5\n"), "{}", body.as_str());
        let mut read = Setup::default();
        assert!(read.read(&body.as_str().replace('\n', " ")).is_ok());
        assert_eq!(read, setup);
        let mut off = Setup::default();
        assert!(off.read("rumble=off mono=off").is_ok());
        assert_eq!(off, Setup::default());
    }

    #[test]
    fn bad_words_are_named() {
        let mut setup = Setup::default();
        assert!(setup.read("curve=nonsense").unwrap_err().as_str().contains("nonsense"));
        assert!(setup.read("bass=20").unwrap_err().as_str().starts_with("bass="));
        assert!(setup.read("mono=maybe").is_err() && setup.read("polarity=sideways").is_err() && setup.read("rumble=5").is_err());
    }

    #[test]
    fn touches_pick_curves_and_flip_switches() {
        let layout = Layout::new(1366, 768);
        let mut setup = Setup::default();
        let rect = layout.widget_rect(Hit::Curve(3)).unwrap();
        let hit = layout.hit(rect.x + 2, rect.y + 2).unwrap();
        assert_eq!(hit, Hit::Curve(3));
        press(&layout, &mut setup, hit, 0);
        assert!(lit(&setup, Hit::Curve(3)) && !lit(&setup, Hit::Curve(0)));
        press(&layout, &mut setup, Hit::Mono, 0);
        press(&layout, &mut setup, Hit::InvertLeft, 0);
        assert!(setup.mono && setup.polarity == Polarity::InvertLeft);
        press(&layout, &mut setup, Hit::InvertRight, 0);
        assert_eq!(setup.polarity, Polarity::InvertBoth);
        press(&layout, &mut setup, Hit::InvertLeft, 0);
        assert_eq!(setup.polarity, Polarity::InvertRight);
    }

    #[test]
    fn sliders_cover_their_ranges() {
        let layout = Layout::new(1280, 1024);
        let mut setup = Setup::default();
        for (index, lo, hi) in [(0u8, 0.0, 12.0), (1, 60.0, 150.0), (2, 0.3, 3.0), (3, 0.0, 100.0), (4, -24.0, 12.0)] {
            let rect = layout.widget_rect(Hit::Slider(index)).unwrap();
            let value = |setup: &Setup| [setup.bass.gain_db, setup.bass.hz, setup.bass.q, setup.rumble_hz, setup.trim_db][index as usize];
            drag(&layout, &mut setup, Hit::Slider(index), rect.x - 50);
            assert!((value(&setup) - lo).abs() < 0.01, "slider {index} low {}", value(&setup));
            drag(&layout, &mut setup, Hit::Slider(index), rect.x + rect.w + 50);
            assert!((value(&setup) - hi).abs() < 0.5, "slider {index} high {}", value(&setup));
            let back = slider_value(&setup, index);
            assert!((back - 1.0).abs() < 0.01, "slider {index} position {back}");
        }
    }

    #[test]
    fn trim_has_a_wide_zero_and_small_steps_beside_it() {
        let mut setup = Setup::default();
        let (below, _) = trim_zero_zone();
        for position in [below + 0.002, below + TRIM_ZERO_HALF, below + 2.0 * TRIM_ZERO_HALF] {
            set_slider(&mut setup, 4, position);
            assert_eq!(setup.trim_db, 0.0, "{position}");
        }
        set_slider(&mut setup, 4, below + 2.0 * TRIM_ZERO_HALF + 0.003);
        assert!(setup.trim_db > 0.05 && setup.trim_db < 0.3, "{}", setup.trim_db);
        set_slider(&mut setup, 4, below - 0.003);
        assert!(setup.trim_db < -0.05 && setup.trim_db > -0.3, "{}", setup.trim_db);
        for db in [-24.0, -5.5, 0.0, 0.1, 7.0, 12.0] {
            setup.trim_db = db;
            let position = slider_value(&setup, 4);
            let mut back = Setup::default();
            set_slider(&mut back, 4, position);
            assert!((back.trim_db - db).abs() < 0.01, "{db} -> {position} -> {}", back.trim_db);
        }
    }

    #[test]
    fn applying_the_setup_shapes_the_response() {
        let mut eq = Phono::new(48000);
        let setup = Setup { bass: BassBoost { gain_db: 6.0, hz: 100.0, q: 1.0 }, ..Setup::default() };
        setup.apply(&mut eq);
        let plain = Phono::new(48000);
        assert!(eq.response_db(100.0) > plain.response_db(100.0) + 5.5);
    }
}
