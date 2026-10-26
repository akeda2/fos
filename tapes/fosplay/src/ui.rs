//! fosplay's touch screen: title, a progress bar that seeks, transport buttons, the playlist, a level meter, the volume and
//! the output effects. `Ui` draws what `View` says and turns finger events into `Action`s; `Output` (main.rs) carries them out,
//! so the picture never decides anything about the sound.

use audio_screen::Screen;
use core::fmt::Write;
use gfx::Canvas;
use instrument_rt::{Rect, TouchTarget, Touches, blit_banded};
use tape_rt::{Line, TapeApi};

const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const MAX_ROWS: usize = 24;
const MAX_WIDGETS: usize = MAX_ROWS + 16;
const BACKGROUND: u32 = 0x0c1014;
const EDGE: u32 = 0x4a5a68;
const ACTIVE: u32 = 0xffb347;
const BUTTON_OFF: u32 = 0x2a343e;
const INK: u32 = 0xe4ecf0;
const DIM: u32 = 0x7d8791;
const BAR: u32 = 0x1c6a8a;
const GREEN: u32 = 0x3ccf6a;
const AMBER: u32 = 0xffc83d;
const RED: u32 = 0xff4d4d;
/// The volume slider spans this many dB below full scale; its first hundredth is silence.
pub const VOLUME_RANGE_DB: f32 = 50.0;
/// The bass slider's top, as `bass=`.
pub const BASS_MAX_DB: f32 = 12.0;

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
    Width,
    Upmix,
    Center,
}

/// What a finger asked for. Sliders send their value (0..1, `Bass` in dB), `Seek` a permille of the track.
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
    Width(f32),
    Upmix,
    Center,
}

/// What the screen shows; `Output` and `play_track` keep it current.
pub struct View {
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
    pub width: f32,
    pub upmix: bool,
    /// The device has the six outputs the upmix needs.
    pub upmix_ok: bool,
    pub center: bool,
    pub message: Line<96>,
}

impl View {
    fn new() -> Self {
        View {
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
            width: 0.7,
            upmix: false,
            upmix_ok: false,
            center: true,
            message: Line::new(),
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
    /// Header and progress bar on top, the transport row under them; below, the playlist on the left (rows, then page
    /// buttons) and on the right the meter, three sliders and the two upmix switches.
    pub fn new(width: i32, height: i32) -> Self {
        let scale = if width >= 1200 { 2 } else { 1 };
        let margin = 6 * scale;
        let gap = margin / 2;
        let text = GLYPH_H * scale;
        let header_h = text * 3 + margin;
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
        layout.add(Rect { x: margin, y, w: width - 2 * margin, h: text * 2 }, Hit::Progress);
        y += text * 2 + margin;
        let buttons = [Hit::Previous, Hit::Back, Hit::PlayPause, Hit::Forward, Hit::Next, Hit::Stop];
        let cell = (width - 2 * margin - 5 * gap) / 6;
        for (index, hit) in buttons.into_iter().enumerate() {
            layout.add(Rect { x: margin + index as i32 * (cell + gap), y, w: cell, h: text * 3 }, hit);
        }
        y += text * 3 + margin;
        let body_h = height - y - margin;
        let list_w = (width - 3 * margin) * 58 / 100;
        let row_h = text * 2 + gap;
        let page_h = text * 3;
        layout.rows = (((body_h - page_h - gap) / row_h).max(1) as usize).min(MAX_ROWS);
        for row in 0..layout.rows {
            layout.add(Rect { x: margin, y: y + row as i32 * row_h, w: list_w, h: row_h - gap }, Hit::Row(row as u8));
        }
        let page_y = y + body_h - page_h;
        let half = (list_w - gap) / 2;
        layout.add(Rect { x: margin, y: page_y, w: half, h: page_h }, Hit::PageUp);
        layout.add(Rect { x: margin + half + gap, y: page_y, w: half, h: page_h }, Hit::PageDown);
        let right_x = margin + list_w + margin;
        let right_w = width - right_x - margin;
        // Five equal cells down the right: the meter, three sliders, the two switches side by side.
        let cell_h = (body_h - 4 * gap) / 5;
        layout.meter = Rect { x: right_x, y, w: right_w, h: cell_h };
        let mut cell_y = y + cell_h + gap;
        for hit in [Hit::Volume, Hit::Bass, Hit::Width] {
            layout.add(Rect { x: right_x, y: cell_y, w: right_w, h: cell_h }, hit);
            cell_y += cell_h + gap;
        }
        let half = (right_w - gap) / 2;
        layout.add(Rect { x: right_x, y: cell_y, w: half, h: cell_h }, Hit::Upmix);
        layout.add(Rect { x: right_x + half + gap, y: cell_y, w: half, h: cell_h }, Hit::Center);
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
    scroll: usize,
    count: usize,
    actions: &'a mut Actions,
    seek: &'a mut Option<u32>,
    scroll_by: &'a mut i32,
}

impl Target<'_> {
    fn slide(&mut self, hit: Hit, x: i32) {
        let value = self.layout.fraction(hit, x);
        match hit {
            Hit::Volume => self.actions.push(Action::Volume(value)),
            Hit::Bass => self.actions.push(Action::Bass(value * BASS_MAX_DB)),
            Hit::Width => self.actions.push(Action::Width(value)),
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
            Hit::PageUp => *self.scroll_by -= self.layout.rows as i32,
            Hit::PageDown => *self.scroll_by += self.layout.rows as i32,
            Hit::Row(row) => {
                let index = self.scroll + row as usize;
                if index < self.count {
                    self.actions.push(Action::Jump(index));
                }
            }
            Hit::Progress | Hit::Volume | Hit::Bass | Hit::Width => self.slide(hit, x),
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
    layout: Layout,
    touches: Touches<Hit>,
    pub view: View,
    scroll: usize,
    /// Where a finger on the progress bar would seek to, shown while it is down.
    seek: Option<u32>,
    actions: Actions,
    next_animation: u64,
    /// The index the list was last scrolled to follow.
    followed: usize,
}

impl Ui {
    /// Takes tape screen `number`; `None` when there is no pixel screen (BIOS text mode) or no memory.
    pub fn open(api: &'static TapeApi, number: u32) -> Option<Ui> {
        let (width, height) = tape_rt::gfx_info_on(number)?;
        let layout = Layout::new(width.min(MAX_WIDTH) as i32, height.min(MAX_HEIGHT) as i32);
        let screen = Screen::new(api, layout.width as u32, layout.height as u32)?.on(number);
        if !screen.open() {
            return None;
        }
        Some(Ui {
            touches: Touches::new(layout.width, layout.height, number),
            screen,
            layout,
            view: View::new(),
            scroll: 0,
            seek: None,
            actions: Actions { queue: [None; 8] },
            next_animation: 0,
            followed: usize::MAX,
        })
    }

    pub fn close(&self) {
        self.screen.close();
    }

    /// Handles the finger events and returns the next thing a finger asked for.
    pub fn next_action(&mut self) -> Option<Action> {
        let mut scroll_by = 0;
        let changed = self.touches.poll(&mut Target {
            layout: &self.layout,
            scroll: self.scroll,
            count: self.view.count,
            actions: &mut self.actions,
            seek: &mut self.seek,
            scroll_by: &mut scroll_by,
        });
        if scroll_by != 0 {
            let moved = self.scroll as i32 + scroll_by;
            self.scroll = clamp_scroll(moved.max(0) as usize, self.layout.rows, self.view.count);
            // The list stays where the finger put it until the track changes.
            self.followed = self.view.index;
        }
        if changed {
            self.screen.mark_dirty();
        }
        self.actions.pop()
    }

    /// True when a frame should be drawn now: something changed, or the meter and clock are due.
    pub fn due(&mut self) -> bool {
        let now = tape_rt::micros();
        let interval = if self.view.paused { 500_000 } else { 100_000 };
        if now >= self.next_animation {
            self.next_animation = now + interval;
            self.screen.mark_dirty();
        }
        self.screen.due()
    }

    /// Draws the picture and sends it. `name(i)` is entry `i` of the playlist.
    pub fn draw<'n>(&mut self, name: impl Fn(usize) -> &'n str) {
        if self.followed != self.view.index {
            self.followed = self.view.index;
            let rows = self.layout.rows;
            self.scroll = clamp_scroll(self.view.index.saturating_sub(rows / 3), rows, self.view.count);
        }
        let scroll = self.scroll;
        {
            let (layout, view, seek) = (&self.layout, &self.view, self.seek);
            let mut canvas = self.screen.frame();
            canvas.clear(BACKGROUND);
            draw_header(&mut canvas, layout, view);
            draw_progress(&mut canvas, layout, view, seek);
            draw_buttons(&mut canvas, layout, view);
            draw_list(&mut canvas, layout, view, scroll, name);
            draw_meter(&mut canvas, layout, view);
            draw_controls(&mut canvas, layout, view);
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
    let title = if view.title.is_empty() { "fosplay" } else { view.title.as_str() };
    canvas.text_scaled(rect.x, rect.y, title, scale as u32, INK);
    canvas.text_scaled(rect.x, rect.y + text, view.detail.as_str(), scale as u32, DIM);
    let mut line = Line::<96>::new();
    if !view.message.is_empty() {
        let _ = write!(line, "{}", view.message.as_str());
    } else {
        let _ = write!(line, "{}", view.format.as_str());
    }
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

fn draw_buttons(canvas: &mut Canvas, layout: &Layout, view: &View) {
    let labels = [
        (Hit::Previous, "|<"),
        (Hit::Back, "-10s"),
        (Hit::PlayPause, if view.paused { "Play" } else { "Pause" }),
        (Hit::Forward, "+10s"),
        (Hit::Next, ">|"),
        (Hit::Stop, "Stop"),
    ];
    for (hit, label) in labels {
        let rect = layout.rect(hit);
        let lit = hit == Hit::PlayPause && view.paused;
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if lit { ACTIVE } else { BUTTON_OFF });
        canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
        center(canvas, rect, label, layout.scale, if lit { 0x000000 } else { INK });
    }
}

fn draw_list<'n>(canvas: &mut Canvas, layout: &Layout, view: &View, scroll: usize, name: impl Fn(usize) -> &'n str) {
    for row in 0..layout.rows {
        let index = scroll + row;
        let rect = layout.rect(Hit::Row(row as u8));
        if index >= view.count {
            break;
        }
        let current = index == view.index;
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if current { ACTIVE } else { BUTTON_OFF });
        let mut line = Line::<96>::new();
        let _ = write!(line, "{:>2} {}", index + 1, name(index));
        let y = rect.y + (rect.h - GLYPH_H * layout.scale) / 2;
        canvas.text_scaled(rect.x + layout.margin, y, line.as_str(), layout.scale as u32, if current { 0x000000 } else { INK });
    }
    for (hit, label) in [(Hit::PageUp, "Up"), (Hit::PageDown, "Down")] {
        let rect = layout.rect(hit);
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
        canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
        center(canvas, rect, label, layout.scale, INK);
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
    canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * value.clamp(0.0, 1.0)) as u32, rect.h as u32, BAR);
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
    center(canvas, rect, label, layout.scale, if enabled { INK } else { DIM });
}

fn switch(canvas: &mut Canvas, layout: &Layout, hit: Hit, on: bool, label: &str, enabled: bool) {
    let rect = layout.rect(hit);
    canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { ACTIVE } else { BUTTON_OFF });
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
    center(canvas, rect, label, layout.scale, if on { 0x000000 } else if enabled { INK } else { DIM });
}

fn draw_controls(canvas: &mut Canvas, layout: &Layout, view: &View) {
    let mut text = Line::<48>::new();
    if view.volume < 0.01 {
        let _ = write!(text, "Volume: off");
    } else {
        let _ = write!(text, "Volume: {} dB", ((view.volume - 1.0) * VOLUME_RANGE_DB) as i32);
    }
    slider(canvas, layout, Hit::Volume, view.volume, text.as_str(), true);
    text.clear();
    let _ = write!(text, "Bass boost: {} dB", (view.bass_db + 0.5) as i32);
    slider(canvas, layout, Hit::Bass, view.bass_db / BASS_MAX_DB, text.as_str(), true);
    text.clear();
    let _ = write!(text, "Surround: {}%", (view.width * 100.0 + 0.5) as i32);
    slider(canvas, layout, Hit::Width, view.width, text.as_str(), view.upmix_ok);
    switch(canvas, layout, Hit::Upmix, view.upmix, if view.upmix_ok { "5.1 upmix" } else { "5.1: no" }, view.upmix_ok);
    switch(canvas, layout, Hit::Center, view.center, "Centre", view.upmix_ok);
}
