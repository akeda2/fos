//! A bank of programme peak meters drawn as horizontal bars, one row per channel, a page of rows at a time.
//!
//! The detector itself is `audio_wire::meter`; this is the display: a 61-cell bar from -60 dBFS to full scale (one
//! cell per dB) on the Nordic scale (TEST = -18 dBFS) or plain dBFS, a three-second peak hold marker, a latched
//! clip indicator and a number. Only what changed since the last look is sent, a few rows per call, because every
//! byte drawn costs time the audio loop needs.
//!
//! The bank knows nothing about where levels come from: `update` asks a closure for each visible row's reading, so a
//! stream, a recorder or a test can feed it.

use crate::{Text, at, begin_batch, draw, end_batch};
use audio_wire::meter;
use core::fmt::Write;

/// Column of the first bar cell.
pub const BAR_COL: usize = 10;
/// Cells 0..=60, one per dB from -60 dBFS to full scale.
pub const CELLS: i32 = 61;
pub const NUMBER_COL: usize = 72;
pub const CLIP_COL: usize = 79;
/// The Nordic scale's TEST (0) is -18 dBFS: cell 42. The permitted maximum level (+9) is -9 dBFS: cell 51.
pub const TEST_CELL: i32 = 42;
pub const PML_CELL: i32 = 51;
pub const HOLD_MICROS: u64 = 3_000_000;
/// Rows drawn per `update` call unless `set_rows_per_call` says otherwise: each call is short, and a whole page of 12 is
/// refreshed in four. A caller that can spare more time per call (a recorder, whose loop is not timing-critical to the
/// millisecond) asks for more.
const ROWS_PER_CALL: usize = 3;

pub struct MeterBank<const N: usize> {
    /// The screen row of the scale (it takes two rows) and of the first meter row.
    scale_row: usize,
    first_row: usize,
    page_rows: usize,
    rows_per_call: usize,
    rows: usize,
    labels: [Text<8>; N],
    page: usize,
    /// The scale and the numbers in Nordic units (TEST = 0) instead of dBFS.
    nordic: bool,
    /// Rows still to be drawn from scratch; the drawing is spread over many calls so that none holds the audio up.
    needs_full: [bool; N],
    footer_dirty: bool,
    cursor: usize,
    shown_cell: [i16; N],
    shown_hold: [i16; N],
    shown_number: [i16; N],
    shown_clip: [bool; N],
    hold: [u32; N],
    hold_at: [u64; N],
    clip_at: [u64; N],
}

impl<const N: usize> MeterBank<N> {
    /// A bank whose scale is drawn on `scale_row` and `scale_row + 1` and whose rows start at `first_row`, showing
    /// `page_rows` channels at a time (the footer takes the three rows after them).
    pub const fn new(scale_row: usize, first_row: usize, page_rows: usize) -> Self {
        MeterBank {
            scale_row, first_row, page_rows, rows_per_call: ROWS_PER_CALL, rows: 0, labels: [Text::new(); N], page: 0, nordic: true, needs_full: [true; N], footer_dirty: true, cursor: 0,
            shown_cell: [-1; N], shown_hold: [-1; N], shown_number: [i16::MIN; N], shown_clip: [false; N], hold: [0; N], hold_at: [0; N], clip_at: [0; N],
        }
    }

    /// Forgets every row and all display state (a new run).
    pub fn clear(&mut self) {
        let rows_per_call = self.rows_per_call;
        *self = Self::new(self.scale_row, self.first_row, self.page_rows);
        self.rows_per_call = rows_per_call;
    }

    /// How many rows one `update` call draws (all in one write to the console).
    pub fn set_rows_per_call(&mut self, rows: usize) {
        self.rows_per_call = rows.max(1);
    }

    /// Adds a row labelled `label` (up to 8 characters); false if the bank is full.
    pub fn add_row(&mut self, label: &str) -> bool {
        if self.rows == N {
            return false;
        }
        self.labels[self.rows] = Text::from(label);
        self.rows += 1;
        true
    }

    /// Changes a row's label (it is redrawn on the next `update`).
    pub fn set_label(&mut self, row: usize, label: &str) {
        if row < self.rows && self.labels[row].as_str() != label {
            self.labels[row] = Text::from(label);
            self.needs_full[row] = true;
        }
    }

    pub fn rows(&self) -> usize {
        self.rows
    }

    pub fn page_rows(&self) -> usize {
        self.page_rows
    }

    /// The first screen row below the meters (the footer starts here).
    pub fn footer_row(&self) -> usize {
        self.first_row + self.page_rows
    }

    pub fn nordic(&self) -> bool {
        self.nordic
    }

    /// Everything is redrawn from scratch over the next calls (new page, new scale).
    pub fn mark_all(&mut self) {
        self.needs_full = [true; N];
        self.footer_dirty = true;
        self.cursor = 0;
    }

    fn pages(&self) -> usize {
        self.rows.div_ceil(self.page_rows).max(1)
    }

    pub fn next_page(&mut self) {
        self.page = (self.page + 1) % self.pages();
        self.mark_all();
    }

    pub fn previous_page(&mut self) {
        let pages = self.pages();
        self.page = (self.page + pages - 1) % pages;
        self.mark_all();
    }

    /// Switches between the Nordic scale and dBFS and redraws the scale.
    pub fn toggle_scale(&mut self) {
        self.nordic = !self.nordic;
        self.mark_all();
        self.draw_scale();
    }

    /// Clears every peak hold and clip indicator.
    pub fn reset_hold(&mut self) {
        self.hold = [0; N];
        self.clip_at = [0; N];
    }

    /// Blanks the meter rows and, when `note` is given, writes it on the first (meters switched off).
    pub fn blank(&self, note: Option<&str>) {
        for slot in 0..self.page_rows {
            at(self.first_row + slot, 1);
            draw!("\x1b[K");
        }
        if let Some(note) = note {
            at(self.first_row, 2);
            draw!("{note}");
        }
    }

    /// True once after a redraw was requested: the caller then calls `draw_footer`.
    pub fn take_footer_dirty(&mut self) -> bool {
        core::mem::replace(&mut self.footer_dirty, false)
    }

    /// The character in cell `cell` of a bar whose level is at `level` (and peak-hold marker at `hold`): filled up to
    /// the level (`=`, and `#` above the permitted maximum), the hold marker `|`, else the scale: `:` at TEST, `.`
    /// every 6 dB.
    fn bar_char(cell: i32, level: i32, hold: i32) -> char {
        if cell <= level {
            if cell > PML_CELL { '#' } else { '=' }
        } else if cell == hold {
            '|'
        } else if cell == TEST_CELL {
            ':'
        } else if cell % 6 == 0 {
            '.'
        } else {
            ' '
        }
    }

    /// A level in tenths of a dB as text of six columns, signed with one decimal.
    fn tenths_text(tenths: i32, out: &mut Text<8>) {
        let magnitude = tenths.unsigned_abs();
        let mut text = Text::<8>::new();
        let _ = write!(text, "{}{}.{}", if tenths < 0 { '-' } else { '+' }, magnitude / 10, magnitude % 10);
        let _ = write!(out, "{:>6}", text.as_str());
    }

    pub fn draw_scale(&self) {
        let row = self.scale_row;
        at(row, 1);
        draw!("\x1b[K");
        for mark in (-42..=18).step_by(6) {
            let cell = (mark + 42) as usize; // the cell is the same on both scales (the Nordic number is dBFS + 18)
            let value = if self.nordic { mark } else { mark - 18 };
            let mut label = Text::<8>::new();
            let _ = write!(label, "{value}");
            let column = (BAR_COL + cell).saturating_sub((label.len().max(1) - 1) / 2).max(1);
            at(row, column);
            draw!("{}", label.as_str());
        }
        at(row + 1, 1);
        draw!("\x1b[K");
        at(row + 1, 2);
        draw!("{}", if self.nordic { "Nordic" } else { "dBFS" });
        for mark in (0..CELLS).step_by(6) {
            at(row + 1, BAR_COL + mark as usize);
            draw!("{}", if mark == TEST_CELL { 'T' } else { '|' });
        }
        at(row + 1, BAR_COL + PML_CELL as usize);
        draw!("M");
    }

    /// The page line, the legend and `hint` (the keys of the panel this is part of), on the three rows below the meters.
    pub fn draw_footer(&self, hint: &str) {
        let first = self.page * self.page_rows;
        let last = (first + self.page_rows).min(self.rows);
        let row = self.footer_row();
        at(row, 2);
        draw!("Channels {}-{} of {}   N: next page   B: previous page\x1b[K", first + 1, last, self.rows);
        at(row + 1, 2);
        draw!("T = TEST level, -18 dBFS   M = maximum permitted, -9 dBFS   # = above it   ! = clipped\x1b[K");
        at(row + 2, 2);
        draw!("{hint}\x1b[K");
    }

    /// Brings the visible rows up to date, a few per call. `read(row)` returns that row's highest reading since the
    /// last look (so a short peak between looks still shows) and whether a clip happened since. Keeps a three-second
    /// peak hold and a latched clip indicator, and draws only what changed.
    pub fn update(&mut self, now: u64, mut read: impl FnMut(usize) -> (u32, bool)) {
        begin_batch(); // the rows of this call go to the console as one write
        let first = self.page * self.page_rows;
        for _ in 0..self.rows_per_call {
            let slot = self.cursor;
            self.cursor = (self.cursor + 1) % self.page_rows;
            let index = first + slot;
            let row = self.first_row + slot;
            if index >= self.rows {
                let flag = index.min(N - 1);
                if self.needs_full[flag] {
                    at(row, 1);
                    draw!("\x1b[K");
                    self.needs_full[flag] = false;
                }
                continue;
            }
            let full = self.needs_full[index];
            self.needs_full[index] = false;
            let (peak, clip) = read(index);
            if clip {
                self.clip_at[index] = now;
            }
            if peak >= self.hold[index] || now > self.hold_at[index] + HOLD_MICROS {
                self.hold[index] = peak;
                self.hold_at[index] = now;
            }
            let tenths = meter::db_tenths(peak);
            let cell = ((tenths + 600).div_euclid(10)).clamp(0, CELLS - 1);
            let hold_cell = ((meter::db_tenths(self.hold[index]) + 600).div_euclid(10)).clamp(0, CELLS - 1);
            let hold_cell = if hold_cell > cell && self.hold[index] >= 64 { hold_cell } else { -1 };
            let number = if peak < 64 { i16::MAX } else { (tenths + if self.nordic { 180 } else { 0 }) as i16 };
            let clipped = now < self.clip_at[index] + HOLD_MICROS && self.clip_at[index] != 0;
            if full {
                at(row, 1);
                draw!("\x1b[K");
                at(row, 2);
                draw!("{}", self.labels[index].as_str());
            }
            let (old_cell, old_hold) = (i32::from(self.shown_cell[index]), i32::from(self.shown_hold[index]));
            if full || old_cell != cell || old_hold != hold_cell {
                let (low, high) = if full {
                    (0, CELLS - 1)
                } else {
                    let mut low = cell.min(old_cell.max(0));
                    let mut high = cell.max(old_cell);
                    for marker in [hold_cell, old_hold] {
                        if marker >= 0 {
                            low = low.min(marker);
                            high = high.max(marker);
                        }
                    }
                    (low.max(0), high.min(CELLS - 1))
                };
                at(row, BAR_COL + low as usize);
                let mut bar = Text::<64>::new();
                for c in low..=high {
                    let _ = write!(bar, "{}", Self::bar_char(c, cell, hold_cell));
                }
                draw!("{}", bar.as_str());
                self.shown_cell[index] = cell as i16;
                self.shown_hold[index] = hold_cell as i16;
            }
            if full || self.shown_number[index] != number {
                at(row, NUMBER_COL);
                if number == i16::MAX {
                    draw!("  -inf");
                } else {
                    let mut text = Text::<8>::new();
                    Self::tenths_text(i32::from(number), &mut text);
                    draw!("{}", text.as_str());
                }
                self.shown_number[index] = number;
            }
            if full || self.shown_clip[index] != clipped {
                at(row, CLIP_COL);
                draw!("{}", if clipped { "!" } else { " " });
                self.shown_clip[index] = clipped;
            }
        }
        end_batch();
    }
}
