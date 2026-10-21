//! `kaffe`: a graphic "coffee is ready" board, the fos version of radiokannan's touch page. A big red clock, a
//! "Kaffet klart!" button that stamps the time the coffee was made and a "Rensa" button that clears the stamp. The stamp is
//! kept in `kaffe.cfg` (`last=YYYY-MM-DDTHH:MM`), so it survives leaving the tape and a reboot, like the page's localStorage.
//! Keys: Enter or Space = Kaffet klart!, c, Delete or Backspace = Rensa, Escape quits. The buttons are plain rectangles
//! (`Layout::button_at`), so a touch or mouse press maps onto them with one call once the kernel reports pointer events.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::dial;
use gfx::theme::theme;
use tape_rt::{Key, KeyDecoder, Line, TapeApi, WallClock, println};

tape_rt::tape_help!(
    br#"kaffe [@NAME] [screen=N]
A coffee board for a kitchen screen: the time in big red digits and two big
buttons. "Kaffet klart!" stamps the time the coffee was made ("Kaffet nybryggt
kl. HH:MM", in yellow); "Rensa" clears it. The stamp is saved in kaffe.cfg and
is still there after a restart (a stamp from an earlier day shows its date).
The time is the machine's clock (date set, ptp settime, or run clock first).
Keys: Enter or Space = Kaffet klart!   c, Delete or Backspace = Rensa
      Escape quits.
"#
);

const GLYPH_W: i32 = gfx::GLYPH_WIDTH as i32;
const GLYPH_H: i32 = gfx::GLYPH_HEIGHT as i32;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
/// How long a pressed button stays dark.
const PRESS_US: u64 = 250_000;
const RED: u32 = 0xff1a1a;
const YELLOW: u32 = 0xffd400;
const BUTTON: u32 = 0xd3d3d3;
const BUTTON_PRESSED: u32 = 0x8a8a8a;

#[derive(Clone, Copy, PartialEq)]
enum Button {
    Done,
    Clear,
}

#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// Where everything goes on a `width` x `height` screen.
struct Layout {
    width: i32,
    height: i32,
    done: Rect,
    clear: Rect,
    /// Text scale of the title line and of the button labels.
    title_scale: i32,
    label_scale: i32,
    /// The clock's digit box and where its row starts.
    digit_w: i32,
    digit_h: i32,
    digit_t: i32,
    clock_y: i32,
    stamp_y: i32,
}

impl Layout {
    fn new(width: i32, height: i32) -> Self {
        let margin = (height / 30).max(4);
        let title_scale = (height / 400).max(1);
        let title_bottom = margin + GLYPH_H * title_scale;
        let button_h = (height / 7).max(24);
        let button_y = title_bottom + margin * 2;
        let gap = margin * 2;
        let done_w = width * 11 / 20;
        let clear_w = width * 5 / 20;
        let left = (width - done_w - clear_w - gap) / 2;
        let digit_h = (height / 4).max(24);
        let digit_w = digit_h * 5 / 9;
        let clock_y = button_y + button_h + margin * 3;
        Self {
            width,
            height,
            done: Rect { x: left, y: button_y, w: done_w, h: button_h },
            clear: Rect { x: left + done_w + gap, y: button_y, w: clear_w, h: button_h },
            title_scale,
            label_scale: (button_h * 6 / 10 / GLYPH_H).min(done_w * 9 / 10 / (13 * GLYPH_W)).max(1),
            digit_w,
            digit_h,
            digit_t: (digit_h / 8).max(2),
            clock_y,
            stamp_y: clock_y + digit_h + margin * 3,
        }
    }

    /// The button under a press at (`x`, `y`), if any.
    fn button_at(&self, x: i32, y: i32) -> Option<Button> {
        if self.done.contains(x, y) {
            Some(Button::Done)
        } else if self.clear.contains(x, y) {
            Some(Button::Clear)
        } else {
            None
        }
    }
}

/// The saved moment: local date and minute.
#[derive(Clone, Copy, PartialEq)]
struct Stamp {
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
}

impl Stamp {
    fn now(clock: &WallClock) -> Self {
        Self { year: clock.year, month: clock.month, day: clock.day, hour: clock.hour, minute: clock.minute }
    }

    /// `YYYY-MM-DDTHH:MM`, as saved.
    fn parse(text: &str) -> Option<Self> {
        let number = |range: core::ops::Range<usize>| text.get(range)?.parse::<u16>().ok();
        let well_formed = text.len() == 16 && text.as_bytes()[4] == b'-' && text.as_bytes()[7] == b'-' && text.as_bytes()[10] == b'T' && text.as_bytes()[13] == b':';
        if !well_formed {
            return None;
        }
        let (month, day, hour, minute) = (number(5..7)?, number(8..10)?, number(11..13)?, number(14..16)?);
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
            return None;
        }
        Some(Self { year: number(0..4)?, month: month as u8, day: day as u8, hour: hour as u8, minute: minute as u8 })
    }

    fn same_day(&self, clock: &WallClock) -> bool {
        (self.year, self.month, self.day) == (clock.year, clock.month, clock.day)
    }
}

fn read_stamp(arguments: &str) -> Option<Stamp> {
    Stamp::parse(tape_rt::arg_value(arguments, "last")?)
}

fn draw_button(canvas: &mut gfx::Canvas, rect: Rect, label: &str, scale: i32, pressed: bool) {
    canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if pressed { BUTTON_PRESSED } else { BUTTON });
    canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, theme().frame);
    let width = gfx::text_width(label) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, label, scale as u32, 0x000000);
}

fn draw(canvas: &mut gfx::Canvas, layout: &Layout, clock: &WallClock, stamp: Option<Stamp>, pressed: Option<Button>) {
    canvas.clear(0x000000);
    let margin = (layout.height / 30).max(4);
    canvas.text_scaled(margin, margin, "Bryggt nytt kaffe? Tryck: Kaffet klart!", layout.title_scale as u32, 0xffffff);
    draw_button(canvas, layout.done, "Kaffet klart!", layout.label_scale, pressed == Some(Button::Done));
    draw_button(canvas, layout.clear, "Rensa", layout.label_scale, pressed == Some(Button::Clear));

    let ghost = dial::ghost(0x000000, RED);
    let (w, h, t) = (layout.digit_w, layout.digit_h, layout.digit_t);
    let colon_w = t * 2;
    let digits = [clock.hour / 10, clock.hour % 10, clock.minute / 10, clock.minute % 10, clock.second / 10, clock.second % 10];
    let total = 6 * w + 2 * colon_w + 5 * (t / 2).max(2);
    let mut x = (layout.width - total) / 2;
    for (index, digit) in digits.into_iter().enumerate() {
        dial::digit(canvas, x, layout.clock_y, w, h, t, Some(digit), RED, ghost);
        x += w + (t / 2).max(2);
        if index == 1 || index == 3 {
            dial::colon(canvas, x + t / 2, layout.clock_y, t, h, RED);
            x += colon_w;
        }
    }

    if let Some(stamp) = stamp {
        let mut front = Line::<40>::new();
        let mut time = Line::<8>::new();
        if stamp.same_day(clock) {
            let _ = write!(front, "Kaffet nybryggt kl. ");
        } else {
            let _ = write!(front, "Kaffet nybryggt {:04}-{:02}-{:02} kl. ", stamp.year, stamp.month, stamp.day);
        }
        let _ = write!(time, "{:02}:{:02}", stamp.hour, stamp.minute);
        let characters = (front.as_str().len() + time.as_str().len()) as i32;
        let scale = (layout.width * 95 / 100 / (characters * GLYPH_W)).clamp(1, (layout.height / 30).max(1));
        let mut at = (layout.width - characters * GLYPH_W * scale) / 2;
        at = canvas.text_scaled(at, layout.stamp_y, front.as_str(), scale as u32, 0xffffff);
        canvas.text_scaled(at, layout.stamp_y, time.as_str(), scale as u32, YELLOW);
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    if tape_rt::fp_level() == 0 {
        return println!("kaffe: needs a hard-float tape build (kernel without FP support?).");
    }
    let arguments = core::str::from_utf8(tape_rt::args()).unwrap_or("");
    let screen_number = tape_rt::screen_arg(arguments);
    let Some((width, height)) = tape_rt::gfx_info_on(screen_number) else {
        return println!("kaffe: no pixel screen (BIOS text mode). Use a UEFI boot or a VBE mode.");
    };
    let config = match tape_rt::config::Config::load(api, "kaffe", arguments) {
        Ok(config) => config,
        Err(reason) => return println!("kaffe: {reason}."),
    };
    if tape_rt::wall_clock().is_none() {
        return println!("kaffe: the machine's clock is not set (date set, or run ptp settime).");
    }
    let mut stamp = read_stamp(config.text());
    let layout = Layout::new(width.min(MAX_WIDTH) as i32, height.min(MAX_HEIGHT) as i32);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(screen_number)) else {
        return println!("kaffe: out of memory.");
    };
    if !screen.open() {
        return println!("kaffe: cannot take the screen.");
    }
    println!("kaffe: Enter or Space = Kaffet klart!, c = Rensa, Escape quits.");
    let mut decoder = KeyDecoder::new();
    let mut pressed: Option<(Button, u64)> = None;
    let mut last_second = u8::MAX;
    loop {
        let mut action = None;
        let keep_going = poll_keys(api, &mut decoder, |key| match key {
            Key::Enter | Key::Char(b' ') => action = Some(Button::Done),
            Key::Char(b'c') | Key::Delete | Key::Backspace => action = Some(Button::Clear),
            _ => {}
        });
        if !keep_going {
            break;
        }
        let Some(clock) = tape_rt::wall_clock() else { continue };
        if let Some(button) = action {
            pressed = Some((button, tape_rt::micros() + PRESS_US));
            stamp = if button == Button::Done { Some(Stamp::now(&clock)) } else { None };
            let mut body = tape_rt::config::Settings::<40>::new();
            if let Some(stamp) = stamp {
                body.put("last", format_args!("{:04}-{:02}-{:02}T{:02}:{:02}", stamp.year, stamp.month, stamp.day, stamp.hour, stamp.minute));
            }
            if let Err(reason) = config.save(api, body.as_str()) {
                println!("kaffe: the stamp was not saved: {reason}.");
            }
            screen.mark_dirty();
        }
        if pressed.is_some_and(|(_, until)| tape_rt::micros() >= until) {
            pressed = None;
            screen.mark_dirty();
        }
        if clock.second != last_second {
            last_second = clock.second;
            screen.mark_dirty();
        }
        if screen.due() {
            let mut canvas = screen.frame();
            draw(&mut canvas, &layout, &clock, stamp, pressed.map(|(button, _)| button));
            screen.present();
        }
        tape_rt::idle();
    }
    screen.close();
    println!("kaffe: stopped.");
}
