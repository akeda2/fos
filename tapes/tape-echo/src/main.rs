//! `tape-echo`: a tape echo in the Roland RE-201 mould, as an effect box: audio in -> tape loop with three playback heads, spring
//! reverb -> audio out, with every knob on the touch screen (and on MIDI controllers). The sound is `crates/tape-echo`; this
//! file runs it: capture and playback on one device, the picture sent in bands with the audio pumped in between.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use tape_echo_tape::{GLYPH_H, Hit, Layout, SLIDERS, drag, label, lit, midi, press, slider_value, tap};
use gfx::Canvas;
use gfx::theme::theme;
use instrument_rt::{Output, Rect, TouchTarget, Touches, blit_banded, poll_midi};
use tape_echo::{HEAD1, HEAD2, HEAD3, TapTempo, TapeEcho};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"tape-echo [device=N] [outdev=N] [screen=N] [rate=HZ] [latency=MS] [in=N]
[out=N]
A tape echo in the Roland RE-201 style, as an effect box: whatever comes in on
the audio device goes through a tape loop and a spring reverb and out again.
Three playback heads sit 1:2:3 along the tape (Head 1-3 switch them); Repeat
is the tape speed (the pitch glides when it changes), Intensity the feedback,
Echo / Dry / Input the levels, Bass / Treble the tone of the echoes, Wow the
tape's speed drift, Saturate the tape's soft clipping, Spread the echoes'
pan. Reverb and Decay are the spring. Freeze loops what is on the tape for
good and shuts the input. Tap (screen, Tab, or controller 65) sets the delay
from your tapping; it is folded into the tape's range.
MIDI controllers: 20-31 the twelve sliders in screen order (left column top
to bottom, then the right), 64 Freeze while held, 65 Tap, 102-104 Head 1-3.
latency=MS is the audio kept queued (10-200, default 40); the input is
delayed by it. in=N / out=N choose the left channel of the pair (1-based).
Keys: f Freeze, 1 2 3 heads, Tab Tap, Left/Right Repeat; Escape quits.
Use headphones or keep the input away from the speakers: the echo feeds back.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const BACKGROUND: u32 = 0x120f0c;
const PANEL: u32 = 0x241d17;
const EDGE: u32 = 0x6a5a48;
const ACTIVE: u32 = 0xffb347;
const BUTTON_OFF: u32 = 0x3a2f24;
const INK: u32 = 0xf0e4d0;
const BAR: u32 = 0x8a5a1c;

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn meter(canvas: &mut Canvas, x: i32, y: i32, w: i32, level: f32) {
    canvas.fill_rect(x, y, w as u32, 4, 0x080b0f);
    canvas.fill_rect(x, y, (w as f32 * level.min(1.0)) as u32, 4, if level > 0.95 { theme().red } else { theme().green });
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, echo: &TapeEcho, output: &Output, midi_messages: u32) {
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, EDGE);
    canvas.text_scaled(header.x + layout.margin, header.y + (header.h - GLYPH_H * layout.scale) / 2, "FOSFOR ECHO", layout.scale as u32, ACTIVE);
    output.draw_scope(canvas, header.x + header.w * 24 / 100, header.y + 3, header.w * 28 / 100, header.h - 6, 0x080b0f, 0x4ade80);
    let mut status = Line::<64>::new();
    let _ = write!(status, "{} ms  {} Hz  MIDI {midi_messages}", (echo.delay() * 1000.0 + 0.5) as i32, output.rate);
    let right = header.x + header.w - layout.margin;
    canvas.text_right(right, header.y + layout.margin, status.as_str(), INK);
    let meter_w = header.w * 14 / 100;
    let y = header.y + header.h - layout.margin - 4;
    meter(canvas, right - 2 * meter_w - layout.margin, y, meter_w, output.input_peak);
    meter(canvas, right - meter_w, y, meter_w, output.peak);
}

fn draw_controls(canvas: &mut Canvas, layout: &Layout, echo: &TapeEcho) {
    let panel = layout.panel;
    canvas.fill_rect(panel.x, panel.y, panel.w as u32, panel.h as u32, PANEL);
    canvas.rect(panel.x, panel.y, panel.w as u32, panel.h as u32, EDGE);
    for widget in layout.widgets() {
        let rect = widget.rect;
        match widget.hit {
            Hit::Slider(index) => {
                let value = slider_value(echo, index);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
                canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * value) as u32, rect.h as u32, BAR);
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                let mut text = Line::<32>::new();
                let _ = write!(text, "{} {}%", SLIDERS[index as usize], (value * 100.0 + 0.5) as i32);
                center(canvas, rect, text.as_str(), layout.scale, INK);
            }
            hit => {
                let on = lit(echo, hit);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { ACTIVE } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                center(canvas, rect, label(hit), layout.scale, if on { 0x000000 } else { INK });
            }
        }
    }
}

struct EchoTouch<'a> {
    layout: &'a Layout,
    echo: &'a mut TapeEcho,
    taps: &'a mut TapTempo,
}

impl TouchTarget for EchoTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        press(self.layout, self.echo, self.taps, hit, x, y, tape_rt::micros());
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, x: i32, y: i32) -> Hit {
        drag(self.layout, self.echo, grab, x, y);
        grab
    }

    fn up(&mut self, _grab: Hit) {}
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let start = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(start) => start,
        Err(reason) => {
            tape_rt::init(api);
            return println!("tape-echo: {reason}.");
        }
    };
    let mut output = match Output::start_duplex(api, start.arguments) {
        Ok(output) => output,
        Err(reason) => return println!("tape-echo: {}.", reason.as_str()),
    };
    let mut echo = TapeEcho::new(output.rate);
    let mut taps = TapTempo::new();

    let layout = Layout::new(start.width as i32, start.height as i32);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(start.screen)) else {
        output.stop();
        return println!("tape-echo: out of memory.");
    };
    if !screen.open() {
        output.stop();
        return println!("tape-echo: cannot take the screen.");
    }
    println!(
        "tape-echo: {} Hz, {} ms queued, in -> echo -> out outputs {}-{}. Escape quits.",
        output.rate,
        output.latency_ms,
        output.first + 1,
        output.first + 2
    );
    // The playback queue holds the latency; the captured frames then refill it one for one.
    output.prime(|frames| frames.fill([0.0; 2]));

    let mut decoder = KeyDecoder::new();
    let mut touches = Touches::new(layout.width, layout.height, start.screen);
    let mut midi_messages = 0u32;
    let mut next_animation = 0u64;
    loop {
        let mut changed = false;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            changed = true;
            match key {
                Key::Char(b'f') => echo.set_freeze(!echo.frozen()),
                Key::Char(c @ b'1'..=b'3') => echo.set_heads(echo.heads() ^ [HEAD1, HEAD2, HEAD3][(c - b'1') as usize]),
                Key::Tab => tap(&mut echo, &mut taps, tape_rt::micros()),
                Key::Left => echo.set_repeat_rate(echo.repeat_rate() - 0.02),
                Key::Right => echo.set_repeat_rate(echo.repeat_rate() + 0.02),
                _ => changed = false,
            }
        });
        if !keep_going {
            break;
        }
        changed |= touches.poll(&mut EchoTouch { layout: &layout, echo: &mut echo, taps: &mut taps });
        let now = tape_rt::micros();
        let accepted = poll_midi(|message| midi(&mut echo, &mut taps, message, now));
        midi_messages += accepted;
        changed |= accepted > 0;

        output.pump_duplex(|frames| echo.process_block(frames));
        if output.input_peak > 0.002 && now >= next_animation {
            next_animation = now + 60_000;
            changed = true;
        }
        if changed {
            screen.mark_dirty();
        }
        if screen.due() {
            {
                let mut canvas = screen.frame();
                canvas.clear(BACKGROUND);
                draw_header(&mut canvas, &layout, &echo, &output, midi_messages);
                draw_controls(&mut canvas, &layout, &echo);
            }
            blit_banded(&screen, || {
                output.pump_duplex(|frames| echo.process_block(frames));
            });
        }
        tape_rt::idle();
    }
    output.stop();
    screen.close();
    println!("tape-echo: stopped.");
}
