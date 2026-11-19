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
    Yes,
    No,
    Nothing,
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
        let strips = Rect::new(body.x, tabs.y + tabs.h + 8 + band, body.w, body.y + body.h - tabs.y - tabs.h - 8 - band);
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

    /// The Analyzer's plots for `view`: the waveform and the spectrum, each inside the room for its axis labels (six
    /// columns at the left, a row under the spectrum). Both: the waveform takes the upper two fifths.
    pub fn analyzer_plots(&self, view: crate::AnView) -> (Option<Rect>, Option<Rect>) {
        let b = self.body;
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

    /// The dB readout under the fader.
    pub fn value(&self, strip: usize, app: &App) -> Option<Rect> {
        let s = self.strip(strip, app)?;
        Some(Rect::new(s.x, s.y + s.h - self.row - 4, s.w, self.row))
    }

    /// The fader's travel (the knob's centre moves over it).
    pub fn fader(&self, strip: usize, app: &App) -> Option<Rect> {
        let s = self.strip(strip, app)?;
        let top = s.y + 2 * self.row + 12 + self.row / 2 + 4;
        let bottom = s.y + s.h - self.row - 4 - self.row / 2 - 4;
        Some(Rect::new(s.x + 4, top, s.w * 3 / 5 - 4, (bottom - top).max(1)))
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
