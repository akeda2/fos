//! Where everything is on the screen, in pixels, and what a point hits. Text is the 8x16 console font, doubled on large
//! screens. Soft keys run down the right edge; the page fills the rest under a two-line header, above a one-line footer.

use crate::{App, Page, SOFT_ACTIONS, SOFT_NAV, SOFT_SLOTS, SoftKind, View};

const GLYPH_W: i32 = 8;
const GLYPH_H: i32 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// What a point on the screen is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Hit {
    Soft(usize),
    /// An output tab (Outputs view).
    Tab(usize),
    /// A strip's name, its fader or its pan control (strips count the sources, then the master).
    Label(usize),
    Fader(usize),
    Pan(usize),
    /// Matrix (source row, output column).
    Cell(usize, usize),
    Control(usize),
    /// Meters: an input's bar or name (the input, 0-based), a control cell under it (control index, input).
    Meter(usize),
    InputControl(usize, usize),
    /// Recorder: an input's meter or name (selects it), its arm button (arms or disarms it), a row of the panel.
    RecMeter(usize),
    Arm(usize),
    RecRow(RecRow),
    /// Recorder: a transport button under the take clock (Record starts a take, Stop stops it).
    Transport(Transport),
    /// Analyzer: a pair button. Meters: an item of the inputs picker (`App::pick_items`).
    AnPair(usize),
    Pick(usize),
    Yes,
    No,
    Nothing,
}

/// The Recorder's transport buttons, side by side under the take clock, as on a tape machine: Record (a red disc) and
/// Stop (a square) are two buttons, so a bounced press never stops the take it started. Play comes after Stop when there
/// is playback.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Record,
    Stop,
}

impl Transport {
    pub const ALL: [Transport; 2] = [Transport::Record, Transport::Stop];
}

/// The rows of the Recorder's panel, top to bottom under the take clock and the transport buttons. Four can be tapped (`tappable`): Project
/// types a project name, Disk and Format step, Inputs arms every input.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RecRow {
    File,
    Project,
    Disk,
    Room,
    Format,
    Inputs,
    Clock,
    Writer,
    Written,
    Warning,
}

impl RecRow {
    pub const ALL: [RecRow; 10] =
        [RecRow::File, RecRow::Project, RecRow::Disk, RecRow::Room, RecRow::Format, RecRow::Inputs, RecRow::Clock, RecRow::Writer, RecRow::Written, RecRow::Warning];

    pub fn tappable(self) -> bool {
        matches!(self, RecRow::Project | RecRow::Disk | RecRow::Format | RecRow::Inputs)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub width: i32,
    pub height: i32,
    /// Font scale (1 or 2), one text column and one text row in pixels.
    pub scale: u32,
    pub col: i32,
    pub row: i32,
    pub header: Rect,
    pub footer: Rect,
    pub body: Rect,
    /// The soft-key column.
    pub keys: Rect,
    key_h: i32,
    pub tabs: Rect,
    pub tab_w: i32,
    pub strips: Rect,
    pub strip_w: i32,
    pub label_w: i32,
    pub cell_w: i32,
    pub cell_h: i32,
    pub control_h: i32,
}

/// The fader's travel: the top 60% covers +6..-12 dB, the next 37% -12..-60 dB, below that is off.
const UPPER: f64 = 0.4;
const OFF: f64 = 0.03;
const KNEE_DB: f64 = -12.0;

/// The level of a fader at `position` (0 bottom, 1 top), snapped to 0.5 dB; `-inf` (off) at the bottom.
pub fn fader_db(position: f64) -> f64 {
    let p = position.clamp(0.0, 1.0);
    let db = if p >= UPPER {
        KNEE_DB + (p - UPPER) / (1.0 - UPPER) * (crate::FADER_MAX_DB - KNEE_DB)
    } else if p >= OFF {
        crate::FADER_MIN_DB + (p - OFF) / (UPPER - OFF) * (KNEE_DB - crate::FADER_MIN_DB)
    } else {
        return f64::NEG_INFINITY;
    };
    let halves = (db * 2.0 + if db < 0.0 { -0.5 } else { 0.5 }) as i64;
    halves as f64 / 2.0
}

/// Where a fader at `db` sits (0 bottom, 1 top).
pub fn fader_position(db: f64) -> f64 {
    if !db.is_finite() || db < crate::FADER_MIN_DB {
        0.0
    } else if db >= KNEE_DB {
        (UPPER + (db - KNEE_DB) / (crate::FADER_MAX_DB - KNEE_DB) * (1.0 - UPPER)).min(1.0)
    } else {
        OFF + (db - crate::FADER_MIN_DB) / (KNEE_DB - crate::FADER_MIN_DB) * (UPPER - OFF)
    }
}

impl Layout {
    pub fn new(width: u32, height: u32) -> Layout {
        let (width, height) = (width as i32, height as i32);
        let scale = if width >= 1800 && height >= 1000 { 2 } else { 1 };
        let (col, row) = (GLYPH_W * scale, GLYPH_H * scale);
        let header = Rect::new(0, 0, width, row * 2 + 8);
        let key_w = col * 11 + 16;
        let keys = Rect::new(width - key_w, header.h, key_w, height - header.h);
        // One key's height is split into the two gaps between the groups (pages and views, actions, Quit).
        let key_h = ((keys.h - 8) / (SOFT_SLOTS as i32 + 1)).min(row * 4);
        let footer = Rect::new(0, height - row - 8, keys.x, row + 8);
        let body = Rect::new(8, header.h, keys.x - 16, footer.y - header.h);
        let tabs = Rect::new(body.x, body.y, body.w, row + 12);
        // One row above the strips names each run of them (inputs, playback, the output's master): the driver may give an input
        // and a playback channel the same name.
        let band = row + 4;
        // Under the strips a band that only swipes them: a finger there never moves a fader.
        let swipe_h = row * 2 + 8;
        let strips = Rect::new(body.x, tabs.y + tabs.h + 8 + band, body.w, body.y + body.h - tabs.y - tabs.h - 8 - band - swipe_h - 6);
        Layout {
            width,
            height,
            scale: scale as u32,
            col,
            row,
            header,
            footer,
            body,
            keys,
            key_h,
            tabs,
            tab_w: col * 12,
            strips,
            strip_w: col * 11,
            label_w: col * 12,
            cell_w: col * 8,
            cell_h: row + 8,
            control_h: row * 2,
        }
    }

    /// Soft key `slot` (`App::soft_keys`): pages with the active page's views indented one column under it, then the
    /// page's actions, then Quit, with half a key between the groups.
    pub fn soft(&self, app: &App, slot: usize) -> Rect {
        let gaps = (slot >= SOFT_NAV) as i32 + (slot >= SOFT_NAV + SOFT_ACTIONS) as i32;
        let indent = if app.soft_keys()[slot].kind == SoftKind::View { self.col } else { 0 };
        let y = self.keys.y + 4 + slot as i32 * self.key_h + gaps * self.key_h / 2;
        Rect::new(self.keys.x + 8 + indent, y, self.keys.w - 16 - indent, self.key_h - 8)
    }

    /// The swipe-only band under the strips (Outputs view), and the group names above them, which swipe too.
    pub fn swipe_band(&self) -> Rect {
        let y = self.strips.y + self.strips.h + 6;
        Rect::new(self.strips.x, y, self.strips.w, self.body.y + self.body.h - y)
    }

    pub fn group_band(&self) -> Rect {
        Rect::new(self.strips.x, self.tabs.y + self.tabs.h + 4, self.strips.w, self.strips.y - self.tabs.y - self.tabs.h - 4)
    }

    /// The Analyzer's row of pair buttons along the top of the page.
    pub fn an_pairs(&self) -> Rect {
        Rect::new(self.body.x, self.body.y + 4, self.body.w, self.row * 2)
    }

    pub fn an_pair_w(&self) -> i32 {
        self.col * 9
    }

    pub fn visible_pairs(&self) -> usize {
        (self.an_pairs().w / self.an_pair_w()).max(1) as usize
    }

    pub fn an_pair(&self, pair: usize, app: &App) -> Option<Rect> {
        let slot = pair.checked_sub(app.first_pair).filter(|&s| s < self.visible_pairs() && pair < app.analysis.pairs())?;
        let row = self.an_pairs();
        Some(Rect::new(row.x + slot as i32 * self.an_pair_w(), row.y, self.an_pair_w() - 6, row.h))
    }

    /// Meters: the inputs picker's items, group presets on one row, then the inputs in a grid, then Done.
    pub fn pick(&self, item: usize, app: &App) -> Option<Rect> {
        let (groups, inputs) = (app.meter_groups().len(), app.metering.channels.max(1));
        let (w, h, gap) = (self.col * 10, self.row * 2, 6);
        let b = self.body;
        let per_row = ((b.w - 16) / (w + gap)).max(1) as usize;
        if item < groups {
            return Some(Rect::new(b.x + 8 + item as i32 * (w + gap), b.y + self.row + 12, w, h));
        }
        let top = b.y + self.row * 5 + 24;
        if item < groups + inputs {
            let n = item - groups;
            return Some(Rect::new(b.x + 8 + (n % per_row) as i32 * (w + gap), top + (n / per_row) as i32 * (h + gap), w, h));
        }
        (item == groups + inputs).then(|| Rect::new(b.x + b.w - w - 8, b.y + b.h - h - 8, w, h))
    }

    /// The Analyzer's plots for `view`: the waveform and the spectrum, each inside the room for its axis labels (six
    /// columns at the left, a row under the spectrum). Both: the waveform takes the upper two fifths. The pair buttons
    /// are above them.
    pub fn analyzer_plots(&self, view: crate::AnView) -> (Option<Rect>, Option<Rect>) {
        let skip = self.an_pairs().h + 8;
        let b = Rect::new(self.body.x, self.body.y + skip, self.body.w, self.body.h - skip);
        let (left, top) = (self.col * 6, self.row);
        let plot = |y: i32, h: i32, under: i32| Rect::new(b.x + left, y + top, b.w - left - self.col, h - top - under);
        match view {
            crate::AnView::Waveform => (Some(plot(b.y, b.h, self.row / 2)), None),
            crate::AnView::Spectrum => (None, Some(plot(b.y, b.h, self.row + 8))),
            crate::AnView::Both => {
                let wave_h = b.h * 2 / 5;
                (Some(plot(b.y, wave_h, self.row / 2)), Some(plot(b.y + wave_h, b.h - wave_h, self.row + 8)))
            }
        }
    }

    /// Meters: the readouts and history of the selected input along the bottom of the page.
    pub fn meter_strip(&self) -> Rect {
        let h = self.row * 3 + 16;
        Rect::new(self.body.x, self.body.y + self.body.h - h, self.body.w, h)
    }

    /// Meters: the area the bars' columns share, inside the room for the scale's labels (`gfx::meter::draw_scale` writes
    /// the 8x16 font, unscaled: the scale at the left, the scale again and the dBFS column at the right), the scale's name
    /// and the clip lamps above.
    pub fn meter_area(&self) -> Rect {
        let b = self.body;
        let (left, right, top) = (48, 160, 36);
        let bottom = self.meter_strip().y - 8;
        Rect::new(b.x + left, b.y + top, b.w - left - right, bottom - b.y - top)
    }

    /// A bar's column width: a wide one for a pair (LUFS), clamped between a readable name and a sensible bar.
    pub fn meter_column_w(&self, app: &App) -> i32 {
        let per = app.metering.per_bar() as i32;
        let area = self.meter_area();
        (area.w / app.metering.bars().max(1) as i32).clamp(self.col * 7 * per, self.col * 10 * per)
    }

    pub fn visible_meters(&self, app: &App) -> usize {
        (self.meter_area().w / self.meter_column_w(app)).max(1) as usize
    }

    /// The whole column of bar `bar` (an input, or a pair in LUFS): the bar, its name, its control cells.
    pub fn meter_column(&self, bar: usize, app: &App) -> Option<Rect> {
        let slot = bar.checked_sub(app.first_meter).filter(|&s| s < self.visible_meters(app) && bar < app.metering.bars())?;
        let (area, w) = (self.meter_area(), self.meter_column_w(app));
        Some(Rect::new(area.x + slot as i32 * w, area.y, w - 6, area.h))
    }

    /// The bars' width: the visible columns (the scale's tick lines run across it).
    pub fn meter_span(&self, app: &App) -> i32 {
        self.visible_meters(app).min(app.metering.bars()) as i32 * self.meter_column_w(app) - 6
    }

    /// Height of one control cell under a bar: one and a half text rows, a finger's width.
    pub fn meter_cell_h(&self) -> i32 {
        self.row * 3 / 2 + 10
    }

    /// The bar of column `bar`, above its name and control cells.
    pub fn meter_bar(&self, bar: usize, app: &App) -> Option<Rect> {
        let c = self.meter_column(bar, app)?;
        let under = self.row + 4 + app.meter_rows() as i32 * self.meter_cell_h();
        Some(Rect::new(c.x, c.y, c.w, (c.h - under).max(8)))
    }

    /// The name row under bar `bar`.
    pub fn meter_name(&self, bar: usize, app: &App) -> Option<Rect> {
        let b = self.meter_bar(bar, app)?;
        Some(Rect::new(b.x, b.y + b.h + 2, b.w, self.row))
    }

    /// Control cell `row` of `input` under its bar (in LUFS the pair's column is split, left input left).
    pub fn meter_cell(&self, input: usize, row: usize, app: &App) -> Option<Rect> {
        let per = app.metering.per_bar();
        let name = self.meter_name(app.metering.bar_of(input)?, app)?;
        let w = (name.w - (per as i32 - 1) * 4) / per as i32;
        let x = name.x + (input % per) as i32 * (w + 4);
        Some(Rect::new(x, name.y + self.row + 2 + row as i32 * self.meter_cell_h(), w, self.meter_cell_h() - 4))
    }

    /// Recorder: the take's status at the left of the page (the clock, the file, the disk, the warnings).
    pub fn rec_panel(&self) -> Rect {
        let w = (self.body.w * 2 / 5).max(self.col * 44).min(self.body.w / 2);
        Rect::new(self.body.x, self.body.y, w, self.body.h)
    }

    /// Recorder: the height of the take's state and clock at the top of the panel (the 8x16 font at three times the
    /// scale). It is only shown, never tapped.
    pub fn rec_big(&self) -> i32 {
        16 * 3 * self.scale as i32
    }

    /// Recorder: a transport button, under the take clock: an equal share of the panel's width each, three text rows tall
    /// (a fingertip).
    pub fn rec_transport(&self, button: Transport) -> Rect {
        let panel = self.rec_panel();
        let (count, gap) = (Transport::ALL.len() as i32, self.col);
        let w = (panel.w - 8 - gap * (count - 1)) / count;
        Rect::new(panel.x + 4 + button as i32 * (w + gap), panel.y + 12 + self.rec_big(), w, self.row * 3)
    }

    /// Recorder: a row of the panel, two text rows tall so a finger can hit the ones that can be tapped.
    pub fn rec_row(&self, row: RecRow) -> Rect {
        let panel = self.rec_panel();
        let height = self.row * 2;
        let buttons = self.rec_transport(Transport::Record);
        let top = buttons.y + buttons.h + self.row / 2;
        Rect::new(panel.x + 4, top + row as i32 * height, panel.w - 8, height - 4)
    }

    /// Recorder: the meters' area at the right, inside the room for the scale's labels (8x16 font, unscaled, as on the
    /// Meters page: the scale left and right of the bars, the dBFS column beside the Nordic scale) and its name above.
    pub fn rec_area(&self) -> Rect {
        let panel = self.rec_panel();
        let (left, right, top) = (48, 110, 36);
        let x = panel.x + panel.w + left;
        Rect::new(x, self.body.y + top, (self.body.x + self.body.w - right - x).max(self.col * 4), self.body.h - top - 8)
    }

    /// One input's column: as wide as the inputs allow, at least an arm button a finger can hit (6 characters: 48 pixels
    /// at scale 1; more inputs than fit are swiped to), at most a broad bar.
    pub fn rec_column_w(&self, app: &App) -> i32 {
        (self.rec_area().w / app.recording.channels.max(1) as i32).clamp(self.col * 6, self.col * 12)
    }

    /// The height of an arm button: three text rows (44 pixels at scale 1), a fingertip.
    fn rec_arm_h(&self) -> i32 {
        self.row * 3 - 4
    }

    pub fn visible_rec(&self, app: &App) -> usize {
        (self.rec_area().w / self.rec_column_w(app)).max(1) as usize
    }

    /// The bars' width: the visible columns.
    pub fn rec_span(&self, app: &App) -> i32 {
        self.visible_rec(app).min(app.recording.channels.max(1)) as i32 * self.rec_column_w(app) - 6
    }

    /// The whole column of `input`: its bar, its name, its arm button.
    pub fn rec_column(&self, input: usize, app: &App) -> Option<Rect> {
        let slot = input.checked_sub(app.first_rec).filter(|&s| s < self.visible_rec(app) && input < app.recording.channels.max(1))?;
        let (area, w) = (self.rec_area(), self.rec_column_w(app));
        Some(Rect::new(area.x + slot as i32 * w, area.y, w - 6, area.h))
    }

    pub fn rec_bar(&self, input: usize, app: &App) -> Option<Rect> {
        let c = self.rec_column(input, app)?;
        let under = self.row + 10 + self.rec_arm_h();
        Some(Rect::new(c.x, c.y, c.w, (c.h - under).max(8)))
    }

    pub fn rec_name(&self, input: usize, app: &App) -> Option<Rect> {
        let b = self.rec_bar(input, app)?;
        Some(Rect::new(b.x, b.y + b.h + 2, b.w, self.row))
    }

    /// The arm button under an input's name: a fingertip (`rec_arm_h` by at least `rec_column_w`).
    pub fn rec_arm(&self, input: usize, app: &App) -> Option<Rect> {
        let n = self.rec_name(input, app)?;
        Some(Rect::new(n.x, n.y + self.row + 4, n.w, self.rec_arm_h()))
    }

    fn rec_hit(&self, app: &App, x: i32, y: i32) -> Hit {
        if let Some(button) = Transport::ALL.into_iter().find(|&button| self.rec_transport(button).contains(x, y)) {
            return Hit::Transport(button);
        }
        if let Some(row) = RecRow::ALL.into_iter().find(|&row| row.tappable() && self.rec_row(row).contains(x, y)) {
            return Hit::RecRow(row);
        }
        for input in 0..app.recording.channels.max(1) {
            if self.rec_arm(input, app).is_some_and(|r| r.contains(x, y)) {
                return Hit::Arm(input);
            }
            if self.rec_column(input, app).is_some_and(|r| r.contains(x, y)) {
                return Hit::RecMeter(input);
            }
        }
        Hit::Nothing
    }

    /// How far a press may wander and still count as a tap (a finger is not still): half a text row.
    pub fn slop(&self) -> i32 {
        (self.row / 2).max(4)
    }

    pub fn visible_tabs(&self) -> usize {
        (self.tabs.w / self.tab_w).max(1) as usize
    }

    pub fn visible_strips(&self) -> usize {
        (self.strips.w / self.strip_w).max(1) as usize
    }

    pub fn visible_rows(&self) -> usize {
        ((self.body.h - self.cell_h) / self.cell_h).max(1) as usize
    }

    pub fn visible_cols(&self) -> usize {
        ((self.body.w - self.label_w) / self.cell_w).max(1) as usize
    }

    pub fn visible_controls(&self) -> usize {
        (self.body.h / self.control_h).max(1) as usize
    }

    pub fn tab(&self, output: usize, app: &App) -> Option<Rect> {
        let slot = output.checked_sub(app.first_tab).filter(|&s| s < self.visible_tabs())?;
        Some(Rect::new(self.tabs.x + slot as i32 * self.tab_w, self.tabs.y, self.tab_w - 6, self.tabs.h))
    }

    /// The whole column of strip `strip`, `None` when scrolled out of view.
    pub fn strip(&self, strip: usize, app: &App) -> Option<Rect> {
        let slot = strip.checked_sub(app.first_strip).filter(|&s| s < self.visible_strips() && strip < app.strip_count())?;
        Some(Rect::new(self.strips.x + slot as i32 * self.strip_w, self.strips.y, self.strip_w - 6, self.strips.h))
    }

    /// The group band above strip `strip`: as wide as the column plus its gap, so a run of strips reads as one band.
    pub fn band(&self, strip: usize, app: &App) -> Option<Rect> {
        let s = self.strip(strip, app)?;
        Some(Rect::new(s.x, s.y - self.row - 4, self.strip_w, self.row))
    }

    pub fn label(&self, strip: usize, app: &App) -> Option<Rect> {
        let s = self.strip(strip, app)?;
        Some(Rect::new(s.x, s.y, s.w, self.row + 8))
    }

    pub fn pan(&self, strip: usize, app: &App) -> Option<Rect> {
        let s = self.strip(strip, app)?;
        Some(Rect::new(s.x + 4, s.y + self.row + 12, s.w - 8, self.row))
    }

    /// The fader: a flat bar filled from the bottom up to the level, its top the level's position (`fader_position`).
    pub fn fader(&self, strip: usize, app: &App) -> Option<Rect> {
        let s = self.strip(strip, app)?;
        let top = s.y + 2 * self.row + 20;
        let bottom = s.y + s.h - 4;
        Some(Rect::new(s.x + 4, top, s.w - 12 - self.col * 2, (bottom - top).max(1)))
    }

    /// The level meter beside the fader.
    pub fn meter(&self, strip: usize, app: &App) -> Option<Rect> {
        let fader = self.fader(strip, app)?;
        let s = self.strip(strip, app)?;
        Some(Rect::new(fader.x + fader.w + 4, fader.y, (s.x + s.w - fader.x - fader.w - 8).max(2), fader.h))
    }

    /// Matrix cell (source row, output column); row or column `usize::MAX` is the header.
    pub fn cell(&self, row: usize, col: usize, app: &App) -> Option<Rect> {
        let y = if row == usize::MAX {
            self.body.y
        } else {
            let slot = row.checked_sub(app.first_row).filter(|&s| s < self.visible_rows())?;
            self.body.y + self.cell_h * (slot as i32 + 1)
        };
        let x = if col == usize::MAX {
            self.body.x
        } else {
            let slot = col.checked_sub(app.first_col).filter(|&s| s < self.visible_cols())?;
            self.body.x + self.label_w + self.cell_w * slot as i32
        };
        Some(Rect::new(x, y, if col == usize::MAX { self.label_w - 4 } else { self.cell_w - 4 }, self.cell_h - 4))
    }

    pub fn control(&self, index: usize, app: &App) -> Option<Rect> {
        let slot = index.checked_sub(app.first_ctl).filter(|&s| s < self.visible_controls())?;
        Some(Rect::new(self.body.x, self.body.y + slot as i32 * self.control_h, self.body.w, self.control_h - 4))
    }

    pub fn dialog(&self) -> Rect {
        let w = (self.col * 56).min(self.width - 40);
        let h = self.row * 7;
        Rect::new((self.width - w) / 2, (self.height - h) / 2, w, h)
    }

    pub fn yes(&self) -> Rect {
        let d = self.dialog();
        Rect::new(d.x + d.w / 2 - self.col * 12, d.y + d.h - self.row * 2 - 8, self.col * 10, self.row * 2)
    }

    pub fn no(&self) -> Rect {
        let d = self.dialog();
        Rect::new(d.x + d.w / 2 + self.col * 2, d.y + d.h - self.row * 2 - 8, self.col * 10, self.row * 2)
    }

    fn meter_hit(&self, app: &App, x: i32, y: i32) -> Hit {
        let m = &app.metering;
        for input in 0..m.channels.min(crate::MAX_CHANNELS) {
            for (row, &index) in app.input_controls(input).iter().enumerate() {
                if self.meter_cell(input, row, app).is_some_and(|r| r.contains(x, y)) {
                    return Hit::InputControl(index, input);
                }
            }
        }
        for bar in 0..m.bars() {
            let Some(column) = self.meter_column(bar, app) else { continue };
            if column.contains(x, y) {
                // In a pair's column the half clicked picks the input.
                let half = (x - column.x) * m.per_bar() as i32 / column.w.max(1);
                let first = m.bar_input(bar).unwrap_or(0);
                return Hit::Meter((first + half as usize).min(m.channels.max(1) - 1));
            }
        }
        Hit::Nothing
    }

    pub fn hit(&self, app: &App, x: i32, y: i32) -> Hit {
        if app.confirm.is_some() {
            return if self.yes().contains(x, y) {
                Hit::Yes
            } else if self.no().contains(x, y) {
                Hit::No
            } else {
                Hit::Nothing
            };
        }
        let keys = app.soft_keys();
        if let Some(slot) = (0..SOFT_SLOTS).find(|&k| keys[k].kind != SoftKind::Empty && self.soft(app, k).contains(x, y)) {
            return Hit::Soft(slot);
        }
        if app.page == Page::Meters && app.picking.is_some() {
            return (0..app.pick_items()).find(|&i| self.pick(i, app).is_some_and(|r| r.contains(x, y))).map_or(Hit::Nothing, Hit::Pick);
        }
        if app.page == Page::Meters {
            return self.meter_hit(app, x, y);
        }
        if app.page == Page::Recorder {
            return self.rec_hit(app, x, y);
        }
        if app.page == Page::Analyzer {
            return (0..app.analysis.pairs()).find(|&p| self.an_pair(p, app).is_some_and(|r| r.contains(x, y))).map_or(Hit::Nothing, Hit::AnPair);
        }
        if app.page != Page::Mixer {
            return Hit::Nothing;
        }
        match app.view {
            View::Outputs => {
                if let Some(tab) = (0..app.outputs().len()).find(|&t| self.tab(t, app).is_some_and(|r| r.contains(x, y))) {
                    return Hit::Tab(tab);
                }
                for strip in 0..app.strip_count() {
                    let Some(column) = self.strip(strip, app) else { continue };
                    if !column.contains(x, y) {
                        continue;
                    }
                    if self.pan(strip, app).is_some_and(|r| r.contains(x, y)) && strip < app.sources().len() {
                        return Hit::Pan(strip);
                    }
                    // The whole column below the pan control moves the fader: a wide target for a finger.
                    let fader = self.fader(strip, app).unwrap_or_default();
                    return if y >= fader.y - self.row { Hit::Fader(strip) } else { Hit::Label(strip) };
                }
                Hit::Nothing
            }
            View::Matrix => {
                for row in 0..app.sources().len() {
                    for col in 0..app.outputs().len() {
                        if self.cell(row, col, app).is_some_and(|r| r.contains(x, y)) {
                            return Hit::Cell(row, col);
                        }
                    }
                }
                Hit::Nothing
            }
            View::Device => (0..app.device_rows()).find(|&c| self.control(c, app).is_some_and(|r| r.contains(x, y))).map_or(Hit::Nothing, Hit::Control),
        }
    }
}
