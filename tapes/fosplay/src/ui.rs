//! fosplay's touch screen, three pages: the player (title, a progress bar that seeks, transport buttons, the playlist, a level
//! meter and the volume), the file browser (folders and audio files of a share or of FOSFS) and the settings (bass boost and
//! the upmix). `Ui` draws what `View` says and turns finger events into `Action`s; `Output` and the browser (main.rs) carry
//! them out, so the picture never decides anything about the sound. Moving between pages is the screen's own business.

use audio_screen::Screen;
use core::fmt::Write;
use gfx::Canvas;
use instrument_rt::{Rect, TouchTarget, Touches, blit_banded};
use tape_rt::{Line, TapeApi};

const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const MAX_ROWS: usize = 32;
const MAX_WIDGETS: usize = MAX_ROWS + 16;
const BACKGROUND: u32 = 0x101010;
const EDGE: u32 = 0x5c5c5c;
const ACTIVE: u32 = 0xffb347;
const BUTTON_OFF: u32 = 0x2e2e2e;
const INK: u32 = 0xf0f0f0;
const DIM: u32 = 0x8c8c8c;
/// The fill of sliders and the progress bar; the screen this runs on shows blues badly, so the theme is red, orange, green.
const BAR: u32 = 0xb8531a;
/// A slider that does nothing in this layout.
const BAR_OFF: u32 = 0x4a3a30;
const STOP: u32 = 0x9c2a2a;
const GREEN: u32 = 0x3ccf6a;
const AMBER: u32 = 0xffc83d;
const RED: u32 = 0xff4d4d;
const FOLDER: u32 = 0x8fe07a;
/// The volume slider spans this many dB below full scale; its first hundredth is silence.
pub const VOLUME_RANGE_DB: f32 = 50.0;
/// The bass slider's top, as `bass=`.
pub const BASS_MAX_DB: f32 = 12.0;
/// The ends of the crossover and delay sliders.
pub const LFE_RANGE: (f32, f32) = (40.0, 200.0);
pub const DELAY_RANGE: (f32, f32) = (0.0, 40.0);
pub const BASS_HZ_RANGE: (f32, f32) = (60.0, 150.0);
pub const BASS_Q_RANGE: (f32, f32) = (0.3, 3.0);

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Page {
    Player,
    Browse,
    Settings,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Hit {
    Previous,
    Back,
    PlayPause,
    Forward,
    Next,
    Stop,
    Progress,
    Row(u8),
    PageUp,
    PageDown,
    Volume,
    Bass,
    BassHz,
    BassQ,
    BassMgmt,
    Width,
    Lfe,
    Delay,
    Upmix,
    Center,
    Layout,
    ToBrowse,
    ToSettings,
    ToPlayer,
    Done,
    Parent,
    PlayAll,
    Quit,
}

/// What a finger asked for. Sliders send their value (0..1, `Bass` in dB, `Lfe` in Hz, `Delay` in ms), `Seek` a permille of
/// the track, `Open` an entry of the browser's list, `Jump` one of the playlist.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Action {
    Previous,
    Back,
    PlayPause,
    Forward,
    Next,
    Stop,
    Seek(u32),
    Jump(usize),
    Volume(f32),
    Bass(f32),
    BassHz(f32),
    BassQ(f32),
    BassMgmt,
    Width(f32),
    Lfe(f32),
    Delay(f32),
    Upmix,
    Center,
    Layout,
    Open(usize),
    Parent,
    PlayAll,
    Quit,
}

/// What the screen shows; `Output`, `play_track` and the browser keep it current.
pub struct View {
    pub page: Page,
    /// A track is loaded, so the player page has something to show.
    pub playing: bool,
    pub title: Line<96>,
    pub detail: Line<96>,
    pub format: Line<96>,
    pub index: usize,
    pub count: usize,
    pub seconds: f32,
    pub total: f32,
    pub paused: bool,
    pub level: [f32; 2],
    pub volume: f32,
    pub bass_db: f32,
    pub bass_hz: f32,
    pub bass_q: f32,
    /// High-pass the fronts at the crossover, the LFE carries their bass.
    pub bass_mgmt: bool,
    /// The layout has an LFE channel / surround channels.
    pub has_lfe: bool,
    pub has_surround: bool,
    pub width: f32,
    pub lfe_hz: f32,
    pub delay_ms: f32,
    pub upmix: bool,
    /// The device has the outputs the layout needs, so the upmix can be switched on.
    pub upmix_ok: bool,
    pub center: bool,
    pub layout: &'static str,
    /// The layout has a centre channel (so the Centre switch means something).
    pub has_center: bool,
    pub message: Line<96>,
    /// The browser's folder: its name and how many entries it lists.
    pub folder: Line<200>,
    pub entries: usize,
    /// The entry the keys act on; `usize::MAX` until a key moves it.
    pub cursor: usize,
}

impl View {
    fn new() -> Self {
        View {
            page: Page::Player,
            playing: false,
            title: Line::new(),
            detail: Line::new(),
            format: Line::new(),
            index: 0,
            count: 0,
            seconds: 0.0,
            total: 0.0,
            paused: false,
            level: [0.0; 2],
            volume: 1.0,
            bass_db: 0.0,
            bass_hz: 100.0,
            bass_q: 0.7,
            bass_mgmt: false,
            has_lfe: true,
            has_surround: true,
            width: 0.7,
            lfe_hz: 100.0,
            delay_ms: 15.0,
            upmix: false,
            upmix_ok: false,
            center: true,
            layout: "5.1",
            has_center: true,
            message: Line::new(),
            folder: Line::new(),
            entries: 0,
            cursor: usize::MAX,
        }
    }
}

#[derive(Clone, Copy)]
struct Widget {
    rect: Rect,
    hit: Hit,
}

pub struct Layout {
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    margin: i32,
    pub header: Rect,
    pub meter: Rect,
    pub rows: usize,
    widgets: [Widget; MAX_WIDGETS],
    count: usize,
}

impl Layout {
    /// Every page has the header on top. The player: the progress bar, the transport row, the playlist with a slim scroll
    /// column and on the right the meter, the volume and the buttons to the other pages. The browser: a row of buttons and the
    /// list across the whole width. The settings: Back, then two columns of sliders and switches.
    pub fn new(width: i32, height: i32, page: Page) -> Self {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let gap = margin / 2;
        let text = GLYPH_H * scale;
        let header_h = text * if page == Page::Player { 3 } else { 2 } + margin;
        let mut layout = Layout {
            width,
            height,
            scale,
            margin,
            header: Rect { x: margin, y: margin, w: width - 2 * margin, h: header_h },
            meter: Rect::ZERO,
            rows: 0,
            widgets: [Widget { rect: Rect::ZERO, hit: Hit::Stop }; MAX_WIDGETS],
            count: 0,
        };
        let mut y = margin + header_h + margin;
        let row = |layout: &mut Layout, y: i32, hits: &[Hit]| {
            let cell = (width - 2 * margin - (hits.len() as i32 - 1) * gap) / hits.len() as i32;
            for (index, hit) in hits.iter().enumerate() {
                layout.add(Rect { x: margin + index as i32 * (cell + gap), y, w: cell, h: text * 3 }, *hit);
            }
        };
        match page {
            Page::Player => {
                layout.add(Rect { x: margin, y, w: width - 2 * margin, h: text * 2 }, Hit::Progress);
                y += text * 2 + margin;
                row(&mut layout, y, &[Hit::Previous, Hit::Back, Hit::PlayPause, Hit::Forward, Hit::Next, Hit::Stop]);
            }
            Page::Browse => row(&mut layout, y, &[Hit::Parent, Hit::PlayAll, Hit::ToPlayer, Hit::ToSettings, Hit::Quit]),
            Page::Settings => row(&mut layout, y, &[Hit::Done]),
        }
        y += text * 3 + margin;
        let body_h = height - y - margin;
        if page == Page::Settings {
            let column = (width - 3 * margin) / 2;
            let cell_h = (body_h - 5 * gap) / 6;
            let rows = [(Hit::Bass, Some(Hit::Volume)), (Hit::BassHz, Some(Hit::Upmix)), (Hit::BassQ, Some(Hit::Layout)), (Hit::Width, Some(Hit::Center)),
                        (Hit::Lfe, Some(Hit::BassMgmt)), (Hit::Delay, None)];
            for (index, (left, right)) in rows.into_iter().enumerate() {
                let cell_y = y + index as i32 * (cell_h + gap);
                layout.add(Rect { x: margin, y: cell_y, w: column, h: cell_h }, left);
                if let Some(right) = right {
                    layout.add(Rect { x: 2 * margin + column, y: cell_y, w: column, h: cell_h }, right);
                }
            }
            return layout;
        }
        let region_w = if page == Page::Player { (width - 3 * margin) * 62 / 100 } else { width - 2 * margin };
        let scroll_w = text * 2 + margin;
        let list_w = region_w - scroll_w - gap;
        let row_h = text * 2 + gap;
        layout.rows = ((body_h / row_h).max(1) as usize).min(MAX_ROWS);
        for row in 0..layout.rows {
            layout.add(Rect { x: margin, y: y + row as i32 * row_h, w: list_w, h: row_h - gap }, Hit::Row(row as u8));
        }
        let half = (body_h - gap) / 2;
        let scroll_x = margin + list_w + gap;
        layout.add(Rect { x: scroll_x, y, w: scroll_w, h: half }, Hit::PageUp);
        layout.add(Rect { x: scroll_x, y: y + half + gap, w: scroll_w, h: half }, Hit::PageDown);
        if page == Page::Player {
            let right_x = margin + region_w + margin;
            let right_w = width - right_x - margin;
            let cell_h = (body_h - 3 * gap) / 4;
            layout.meter = Rect { x: right_x, y, w: right_w, h: cell_h };
            layout.add(Rect { x: right_x, y: y + cell_h + gap, w: right_w, h: cell_h }, Hit::Volume);
            layout.add(Rect { x: right_x, y: y + 2 * (cell_h + gap), w: right_w, h: cell_h }, Hit::ToBrowse);
            layout.add(Rect { x: right_x, y: y + 3 * (cell_h + gap), w: right_w, h: cell_h }, Hit::ToSettings);
        }
        layout
    }

    fn add(&mut self, rect: Rect, hit: Hit) {
        self.widgets[self.count] = Widget { rect, hit };
        self.count += 1;
    }

    fn widgets(&self) -> &[Widget] {
        &self.widgets[..self.count]
    }

    pub fn rect(&self, hit: Hit) -> Rect {
        self.widgets().iter().find(|widget| widget.hit == hit).map_or(Rect::ZERO, |widget| widget.rect)
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<Hit> {
        self.widgets().iter().find(|widget| widget.rect.contains(x, y)).map(|widget| widget.hit)
    }

    /// Where along `hit`'s rectangle `x` is, 0..=1.
    pub fn fraction(&self, hit: Hit, x: i32) -> f32 {
        let rect = self.rect(hit);
        ((x - rect.x) as f32 / rect.w.max(1) as f32).clamp(0.0, 1.0)
    }
}

/// The first rows of the list to show so that `index` is visible, given the scroll the finger left.
pub fn clamp_scroll(scroll: usize, rows: usize, count: usize) -> usize {
    scroll.min(count.saturating_sub(rows))
}

/// Pending actions; one of a kind at a time (a slider dragged between two polls sends its last value only).
struct Actions {
    queue: [Option<Action>; 8],
}

impl Actions {
    fn push(&mut self, action: Action) {
        if let Some(last) = self.queue.iter_mut().rev().find(|slot| slot.is_some()) {
            if core::mem::discriminant(&last.unwrap()) == core::mem::discriminant(&action) {
                *last = Some(action);
                return;
            }
        }
        if let Some(free) = self.queue.iter_mut().find(|slot| slot.is_none()) {
            *free = Some(action);
        }
    }

    fn pop(&mut self) -> Option<Action> {
        let first = self.queue[0].take();
        self.queue.rotate_left(1);
        first
    }
}

struct Target<'a> {
    layout: &'a Layout,
    page: Page,
    playing: bool,
    /// The page Done returns to.
    back: Page,
    scroll: usize,
    /// How many rows the page's list has.
    count: usize,
    actions: &'a mut Actions,
    seek: &'a mut Option<u32>,
    scroll_by: &'a mut i32,
    goto: &'a mut Option<Page>,
}

impl Target<'_> {
    fn slide(&mut self, hit: Hit, x: i32) {
        let value = self.layout.fraction(hit, x);
        let between = |(low, high): (f32, f32)| low + value * (high - low);
        match hit {
            Hit::Volume => self.actions.push(Action::Volume(value)),
            Hit::Bass => self.actions.push(Action::Bass(value * BASS_MAX_DB)),
            Hit::BassHz => self.actions.push(Action::BassHz(between(BASS_HZ_RANGE))),
            Hit::BassQ => self.actions.push(Action::BassQ(between(BASS_Q_RANGE))),
            Hit::Width => self.actions.push(Action::Width(value)),
            Hit::Lfe => self.actions.push(Action::Lfe(between(LFE_RANGE))),
            Hit::Delay => self.actions.push(Action::Delay(between(DELAY_RANGE))),
            Hit::Progress => *self.seek = Some((value * 1000.0) as u32),
            _ => {}
        }
    }
}

impl TouchTarget for Target<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        match hit {
            Hit::Previous => self.actions.push(Action::Previous),
            Hit::Back => self.actions.push(Action::Back),
            Hit::PlayPause => self.actions.push(Action::PlayPause),
            Hit::Forward => self.actions.push(Action::Forward),
            Hit::Next => self.actions.push(Action::Next),
            Hit::Stop => self.actions.push(Action::Stop),
            Hit::Upmix => self.actions.push(Action::Upmix),
            Hit::Center => self.actions.push(Action::Center),
            Hit::BassMgmt => self.actions.push(Action::BassMgmt),
            Hit::Layout => self.actions.push(Action::Layout),
            Hit::Parent => self.actions.push(Action::Parent),
            Hit::PlayAll => self.actions.push(Action::PlayAll),
            Hit::Quit => self.actions.push(Action::Quit),
            Hit::ToBrowse => *self.goto = Some(Page::Browse),
            Hit::ToSettings => *self.goto = Some(Page::Settings),
            Hit::ToPlayer if self.playing => *self.goto = Some(Page::Player),
            Hit::ToPlayer => return None,
            Hit::Done => *self.goto = Some(self.back),
            Hit::PageUp => *self.scroll_by -= self.layout.rows as i32,
            Hit::PageDown => *self.scroll_by += self.layout.rows as i32,
            Hit::Row(row) => {
                let index = self.scroll + row as usize;
                if index < self.count {
                    self.actions.push(if self.page == Page::Browse { Action::Open(index) } else { Action::Jump(index) });
                }
            }
            Hit::Progress | Hit::Volume | Hit::Bass | Hit::BassHz | Hit::BassQ | Hit::Width | Hit::Lfe | Hit::Delay => self.slide(hit, x),
        }
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, x: i32, _y: i32) -> Hit {
        self.slide(grab, x);
        grab
    }

    // A seek waits for the finger to lift so one drag is one seek.
    fn up(&mut self, grab: Hit) {
        if grab == Hit::Progress {
            if let Some(permille) = self.seek.take() {
                self.actions.push(Action::Seek(permille));
            }
        }
    }
}

pub struct Ui {
    screen: Screen,
    layouts: [Layout; 3],
    touches: Touches<Hit>,
    pub view: View,
    /// The first row shown, per page (the settings have no list).
    scroll: [usize; 3],
    /// Where a finger on the progress bar would seek to, shown while it is down.
    seek: Option<u32>,
    actions: Actions,
    next_animation: u64,
    /// The index the player's list was last scrolled to follow.
    followed: usize,
    back: Page,
}

fn slot(page: Page) -> usize {
    page as usize
}

impl Ui {
    /// Takes tape screen `number`; `None` when there is no pixel screen (BIOS text mode) or no memory.
    pub fn open(api: &'static TapeApi, number: u32, page: Page) -> Option<Ui> {
        let (width, height) = tape_rt::gfx_info_on(number)?;
        let (width, height) = (width.min(MAX_WIDTH) as i32, height.min(MAX_HEIGHT) as i32);
        let screen = Screen::new(api, width as u32, height as u32)?.on(number);
        if !screen.open() {
            return None;
        }
        let mut view = View::new();
        view.page = page;
        Some(Ui {
            touches: Touches::new(width, height, number),
            screen,
            layouts: [Layout::new(width, height, Page::Player), Layout::new(width, height, Page::Browse), Layout::new(width, height, Page::Settings)],
            view,
            scroll: [0; 3],
            seek: None,
            actions: Actions { queue: [None; 8] },
            next_animation: 0,
            followed: usize::MAX,
            back: page,
        })
    }

    pub fn close(&self) {
        self.screen.close();
    }

    fn layout(&self) -> &Layout {
        &self.layouts[slot(self.view.page)]
    }

    pub fn set_page(&mut self, page: Page) {
        self.view.page = page;
        self.back = page;
        self.screen.mark_dirty();
    }

    /// The settings page, from wherever the panel is now (the key for the Settings button).
    pub fn open_settings(&mut self) {
        let from = self.view.page;
        self.set_page(Page::Settings);
        self.back = from;
    }

    /// A new browser list is on show (a folder was entered): from the top, no entry chosen.
    pub fn reset_list(&mut self) {
        self.scroll[slot(Page::Browse)] = 0;
        self.view.cursor = usize::MAX;
        self.screen.mark_dirty();
    }

    /// Chooses entry `index` of the browser's list and scrolls it to about a third of the way down (coming back out of a
    /// folder, the folder we left is where the finger was).
    pub fn select(&mut self, index: usize) {
        let rows = self.layouts[slot(Page::Browse)].rows;
        self.view.cursor = index;
        self.scroll[slot(Page::Browse)] = clamp_scroll(index.saturating_sub(rows / 3), rows, self.view.entries);
        self.screen.mark_dirty();
    }

    /// Moves the chosen entry of the browser by `delta` (keys), scrolling to keep it in sight.
    pub fn cursor_by(&mut self, delta: i32) {
        let last = self.view.entries.saturating_sub(1) as i32;
        let from = if self.view.cursor == usize::MAX { if delta > 0 { -1 } else { last + 1 } } else { self.view.cursor as i32 };
        let cursor = (from + delta).clamp(0, last) as usize;
        self.view.cursor = cursor;
        let rows = self.layouts[slot(Page::Browse)].rows;
        let scroll = &mut self.scroll[slot(Page::Browse)];
        if cursor < *scroll {
            *scroll = cursor;
        } else if cursor >= *scroll + rows {
            *scroll = cursor + 1 - rows;
        }
        self.screen.mark_dirty();
    }

    /// Handles the finger events and returns the next thing a finger asked for.
    pub fn next_action(&mut self) -> Option<Action> {
        let page = self.view.page;
        let mut scroll_by = 0;
        let mut goto = None;
        let count = if page == Page::Browse { self.view.entries } else { self.view.count };
        let layout = &self.layouts[slot(page)];
        let changed = self.touches.poll(&mut Target {
            layout,
            page,
            playing: self.view.playing,
            back: self.back,
            scroll: self.scroll[slot(page)],
            count,
            actions: &mut self.actions,
            seek: &mut self.seek,
            scroll_by: &mut scroll_by,
            goto: &mut goto,
        });
        if scroll_by != 0 {
            let rows = layout.rows;
            let moved = self.scroll[slot(page)] as i32 + scroll_by;
            self.scroll[slot(page)] = clamp_scroll(moved.max(0) as usize, rows, count);
            // The player's list stays where the finger put it until the track changes.
            self.followed = self.view.index;
        }
        if let Some(next) = goto {
            if next == Page::Settings {
                self.back = page;
            }
            self.view.page = next;
        }
        if changed || goto.is_some() {
            self.screen.mark_dirty();
        }
        self.actions.pop()
    }

    /// True when a frame should be drawn now: something changed, or the meter and clock are due.
    pub fn due(&mut self) -> bool {
        let now = tape_rt::micros();
        let interval = if self.view.paused || self.view.page != Page::Player { 500_000 } else { 100_000 };
        if now >= self.next_animation {
            self.next_animation = now + interval;
            self.screen.mark_dirty();
        }
        self.screen.due()
    }

    /// Draws the picture and sends it. `track(i)` is entry `i` of the playlist, `entry(i)` one of the browser's list.
    pub fn draw<'n>(&mut self, track: impl Fn(usize) -> &'n str, entry: impl Fn(usize) -> &'n str) {
        let page = self.view.page;
        if page == Page::Player && self.followed != self.view.index {
            self.followed = self.view.index;
            let rows = self.layouts[slot(Page::Player)].rows;
            self.scroll[slot(Page::Player)] = clamp_scroll(self.view.index.saturating_sub(rows / 3), rows, self.view.count);
        }
        let scroll = self.scroll[slot(page)];
        {
            let (layout, view, seek) = (&self.layouts[slot(page)], &self.view, self.seek);
            let mut canvas = self.screen.frame();
            canvas.clear(BACKGROUND);
            draw_header(&mut canvas, layout, view);
            match page {
                Page::Player => {
                    draw_progress(&mut canvas, layout, view, seek);
                    draw_transport(&mut canvas, layout, view);
                    draw_list(&mut canvas, layout, view.count, view.index, true, scroll, track);
                    draw_meter(&mut canvas, layout, view);
                    slider(&mut canvas, layout, Hit::Volume, view.volume, volume_text(view).as_str(), true);
                    button(&mut canvas, layout, Hit::ToBrowse, "Browse", false, true);
                    button(&mut canvas, layout, Hit::ToSettings, "Settings", false, true);
                }
                Page::Browse => {
                    for (hit, label, enabled) in [(Hit::Parent, "Parent", true), (Hit::PlayAll, "Play all", true), (Hit::ToPlayer, "Player", view.playing), (Hit::ToSettings, "Settings", true), (Hit::Quit, "Quit", true)] {
                        button(&mut canvas, layout, hit, label, false, enabled);
                    }
                    draw_list(&mut canvas, layout, view.entries, view.cursor, false, scroll, entry);
                }
                Page::Settings => {
                    button(&mut canvas, layout, Hit::Done, "Done", false, true);
                    draw_settings(&mut canvas, layout, view);
                }
            }
        }
        blit_banded(&self.screen, || {});
    }
}

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    let x = rect.x + (rect.w - width).max(0) / 2;
    canvas.text_scaled(x, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn clock(out: &mut impl Write, seconds: f32) {
    let seconds = seconds.max(0.0) as u32;
    let _ = write!(out, "{}:{:02}", seconds / 60, seconds % 60);
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, view: &View) {
    let (rect, scale) = (layout.header, layout.scale);
    let text = GLYPH_H * scale;
    let mut line = Line::<96>::new();
    match view.page {
        Page::Player => {
            let title = if view.title.is_empty() { "fosplay" } else { view.title.as_str() };
            canvas.text_scaled(rect.x, rect.y, title, scale as u32, INK);
            canvas.text_scaled(rect.x, rect.y + text, view.detail.as_str(), scale as u32, DIM);
            let _ = write!(line, "{}", if view.message.is_empty() { view.format.as_str() } else { view.message.as_str() });
            canvas.text_scaled(rect.x, rect.y + 2 * text, line.as_str(), scale as u32, DIM);
            let mut position = Line::<48>::new();
            if view.count > 1 {
                let _ = write!(position, "{}/{}  ", view.index + 1, view.count);
            }
            clock(&mut position, view.seconds);
            let _ = write!(position, " / ");
            clock(&mut position, view.total);
            let width = gfx::text_width(position.as_str()) as i32 * scale;
            canvas.text_scaled(rect.x + rect.w - width, rect.y, position.as_str(), scale as u32, INK);
        }
        Page::Browse => {
            canvas.text_scaled(rect.x, rect.y, view.folder.as_str(), scale as u32, INK);
            if view.message.is_empty() {
                let _ = write!(line, "{} entries", view.entries);
            } else {
                let _ = write!(line, "{}", view.message.as_str());
            }
            canvas.text_scaled(rect.x, rect.y + text, line.as_str(), scale as u32, DIM);
        }
        Page::Settings => {
            canvas.text_scaled(rect.x, rect.y, "Settings", scale as u32, INK);
            let _ = write!(line, "saved when you leave this page");
            canvas.text_scaled(rect.x, rect.y + text, line.as_str(), scale as u32, DIM);
        }
    }
}

fn draw_progress(canvas: &mut Canvas, layout: &Layout, view: &View, seek: Option<u32>) {
    let rect = layout.rect(Hit::Progress);
    canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
    let fraction = match seek {
        Some(permille) => permille as f32 / 1000.0,
        None if view.total > 0.0 => (view.seconds / view.total).clamp(0.0, 1.0),
        None => 0.0,
    };
    canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * fraction) as u32, rect.h as u32, if seek.is_some() { ACTIVE } else { BAR });
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
}

fn button(canvas: &mut Canvas, layout: &Layout, hit: Hit, label: &str, lit: bool, enabled: bool) {
    button_off(canvas, layout, hit, label, lit, enabled, BUTTON_OFF);
}

/// A button whose resting colour is `off` (the Stop button is red while there is something to stop).
fn button_off(canvas: &mut Canvas, layout: &Layout, hit: Hit, label: &str, lit: bool, enabled: bool, off: u32) {
    let rect = layout.rect(hit);
    canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if lit { ACTIVE } else { off });
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
    center(canvas, rect, label, layout.scale, if lit { 0x000000 } else if enabled { INK } else { DIM });
}

fn draw_transport(canvas: &mut Canvas, layout: &Layout, view: &View) {
    let labels = [
        (Hit::Previous, "|<"),
        (Hit::Back, "-10s"),
        (Hit::PlayPause, if view.paused { "Play" } else { "Pause" }),
        (Hit::Forward, "+10s"),
        (Hit::Next, ">|"),
        (Hit::Stop, "Stop"),
    ];
    for (hit, label) in labels {
        button_off(canvas, layout, hit, label, hit == Hit::PlayPause && view.paused, true, if hit == Hit::Stop { STOP } else { BUTTON_OFF });
    }
}

/// The list rows (`current` is lit; the player numbers them, the browser colours folders) and the slim scroll column.
fn draw_list<'n>(canvas: &mut Canvas, layout: &Layout, count: usize, current: usize, numbered: bool, scroll: usize, name: impl Fn(usize) -> &'n str) {
    for row in 0..layout.rows {
        let index = scroll + row;
        if index >= count {
            break;
        }
        let rect = layout.rect(Hit::Row(row as u8));
        let lit = index == current;
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if lit { ACTIVE } else { BUTTON_OFF });
        let label = name(index);
        let mut line = Line::<132>::new();
        if numbered {
            let _ = write!(line, "{:>2} {label}", index + 1);
        } else {
            let _ = write!(line, "{label}");
        }
        let ink = if lit { 0x000000 } else if !numbered && label.ends_with('/') { FOLDER } else { INK };
        let y = rect.y + (rect.h - GLYPH_H * layout.scale) / 2;
        canvas.text_scaled(rect.x + layout.margin, y, line.as_str(), layout.scale as u32, ink);
    }
    for (hit, label) in [(Hit::PageUp, "Up"), (Hit::PageDown, "Dn")] {
        let rect = layout.rect(hit);
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
        canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
        center(canvas, rect, label, 1, INK);
    }
}

/// Two bars (left over right), 60 dB wide.
fn draw_meter(canvas: &mut Canvas, layout: &Layout, view: &View) {
    let rect = layout.meter;
    let half = rect.h / 2;
    for (channel, &level) in view.level.iter().enumerate() {
        let db = if level > 0.0 { dsp::level::amplitude_db(level) } else { -100.0 };
        let fraction = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
        let y = rect.y + channel as i32 * half;
        canvas.fill_rect(rect.x, y, rect.w as u32, (half - 2) as u32, BUTTON_OFF);
        let colour = if db > -1.0 { RED } else if db > -9.0 { AMBER } else { GREEN };
        canvas.fill_rect(rect.x, y, (rect.w as f32 * fraction) as u32, (half - 2) as u32, colour);
    }
}

fn slider(canvas: &mut Canvas, layout: &Layout, hit: Hit, value: f32, label: &str, enabled: bool) {
    let rect = layout.rect(hit);
    canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
    canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * value.clamp(0.0, 1.0)) as u32, rect.h as u32, if enabled { BAR } else { BAR_OFF });
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
    center(canvas, rect, label, layout.scale, if enabled { INK } else { DIM });
}

fn volume_text(view: &View) -> Line<48> {
    let mut text = Line::<48>::new();
    if view.volume < 0.01 {
        let _ = write!(text, "Volume: off");
    } else {
        let _ = write!(text, "Volume: {} dB", ((view.volume - 1.0) * VOLUME_RANGE_DB) as i32);
    }
    text
}

fn draw_settings(canvas: &mut Canvas, layout: &Layout, view: &View) {
    let mut text = Line::<48>::new();
    let ok = view.upmix_ok;
    let surround = ok && view.has_surround;
    let boosted = view.bass_db > 0.0;
    let _ = write!(text, "Bass boost: {} dB", (view.bass_db + 0.5) as i32);
    slider(canvas, layout, Hit::Bass, view.bass_db / BASS_MAX_DB, text.as_str(), true);
    text.clear();
    let _ = write!(text, "Bass centre: {} Hz", (view.bass_hz + 0.5) as i32);
    slider(canvas, layout, Hit::BassHz, (view.bass_hz - BASS_HZ_RANGE.0) / (BASS_HZ_RANGE.1 - BASS_HZ_RANGE.0), text.as_str(), boosted);
    text.clear();
    let _ = write!(text, "Bass width Q: {}.{}", view.bass_q as i32, ((view.bass_q * 10.0 + 0.5) as i32) % 10);
    slider(canvas, layout, Hit::BassQ, (view.bass_q - BASS_Q_RANGE.0) / (BASS_Q_RANGE.1 - BASS_Q_RANGE.0), text.as_str(), boosted);
    text.clear();
    let _ = write!(text, "Surround: {}%", (view.width * 100.0 + 0.5) as i32);
    slider(canvas, layout, Hit::Width, view.width, text.as_str(), surround);
    text.clear();
    let _ = write!(text, "Sub crossover: {} Hz", (view.lfe_hz + 0.5) as i32);
    slider(canvas, layout, Hit::Lfe, (view.lfe_hz - LFE_RANGE.0) / (LFE_RANGE.1 - LFE_RANGE.0), text.as_str(), ok && view.has_lfe);
    text.clear();
    let _ = write!(text, "Surround delay: {} ms", (view.delay_ms + 0.5) as i32);
    slider(canvas, layout, Hit::Delay, (view.delay_ms - DELAY_RANGE.0) / (DELAY_RANGE.1 - DELAY_RANGE.0), text.as_str(), surround);
    slider(canvas, layout, Hit::Volume, view.volume, volume_text(view).as_str(), true);
    text.clear();
    let _ = write!(text, "Upmix {}", if view.upmix { "on" } else if ok { "off" } else { "(too few outputs)" });
    button(canvas, layout, Hit::Upmix, text.as_str(), view.upmix, ok);
    text.clear();
    let _ = write!(text, "Layout: {}", view.layout);
    button(canvas, layout, Hit::Layout, text.as_str(), false, true);
    button(canvas, layout, Hit::Center, "Centre", view.center && view.has_center, ok && view.has_center);
    button(canvas, layout, Hit::BassMgmt, "Fronts to sub", view.bass_mgmt && view.has_lfe, ok && view.has_lfe);
}
