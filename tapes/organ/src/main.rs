//! `organ`: a Hammond-style drawbar organ for the touch screen. Nine drawbars, presets, percussion, scanner vibrato/chorus, a
//! Leslie and overdrive are on the screen and answer to a finger; the 61 keys (C2-C7) answer to a USB-MIDI keyboard, to the
//! on-screen keyboard and to the typed-key keyboard. The sound is `crates/organ`; this file runs it: MIDI in (ABI 25), touch in
//! (ABI 24), stereo audio out, and the picture. The picture is sent in bands with the audio pumped in between, because a
//! full-screen framebuffer write takes longer than the audio buffer holds.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gfx::theme::theme;
use instrument_rt::{Output, TouchTarget, Touches, TypedKeys, blit_banded, poll_midi};
use organ::{DRAWBARS, FIRST_KEY, FOOTAGES, KEYS, Leslie, Organ};
use organ_tape::{GLYPH_H, GLYPH_W, Hit, Layout, Rect, drag, label, lit, note_for_char, press, slider_value};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"organ [device=N|loopN] [outdev=N|loopN]
[screen=N] [rate=HZ] [latency=MS] [out=N]
A drawbar organ in the Hammond B-3 mould. The screen has nine drawbars (drag
them up and down), six presets, percussion (2nd/3rd harmonic, fast/slow,
soft/normal), scanner vibrato V1-V3 and chorus C1-C3, a Leslie (Off, Stop,
Slow, Fast: the rotors speed up and slow down like the real ones), overdrive,
volume, key click, reverb, and a 61-key keyboard that a finger plays. A
USB-MIDI keyboard plays it too (note on/off; controller 123 stops all notes;
the mod wheel sets the Leslie: low Stop, middle Slow, high Fast); while one
is connected the on-screen keyboard is hidden. The sound
is stereo, 32-bit float inside, whatever the audio device takes (24 bit, 48 kHz
USB audio, say). latency=MS is how much audio is kept queued (10-200, default
40): lower is quicker to respond, higher survives a slow screen. out=N picks
the output pair on a multichannel card: N is the left channel (out=9 plays on
outputs 9 and 10; default 1). The other channels stay silent.
Keys: z s x d c v g b h n j m , and q 2 w 3 e r 5 t 6 y 7 u i play notes
(each rings 0.3 s, a terminal sends no key release); Left/Right move them an
octave; Tab flips the Leslie between Slow and Fast; Escape quits.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
/// How long a typed key sounds.
const TYPED_US: u64 = 300_000;
const BACKGROUND: u32 = 0x15110d;
const PANEL: u32 = 0x261d16;
const WOOD_LIGHT: u32 = 0x8a6a45;
const ACTIVE: u32 = 0xffb000;
const BUTTON_OFF: u32 = 0x3d3126;
const INK: u32 = 0xe8dcc0;
const BROWN: u32 = 0x7a3f1c;
const WHITE_BAR: u32 = 0xf0ead8;
const BLACK_BAR: u32 = 0x1a1a1a;

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn bar_colour(index: usize) -> u32 {
    match index {
        0 | 1 => BROWN,
        4 | 6 | 7 => BLACK_BAR,
        _ => WHITE_BAR,
    }
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, organ: &Organ, output: &Output, midi_messages: u32) {
    let (rate, peak) = (output.rate, output.peak);
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, WOOD_LIGHT);
    let scale = layout.scale as u32;
    canvas.text_scaled(header.x + layout.margin, header.y + (header.h - GLYPH_H * layout.scale) / 2, "FOSFOR ORGAN", scale, ACTIVE);

    // The sound itself: the last samples, started at a rising zero crossing so a steady note stands still.
    let scope = Rect { x: header.x + header.w * 28 / 100, y: header.y + 3, w: header.w * 36 / 100, h: header.h - 6 };
    output.draw_scope(canvas, scope.x, scope.y, scope.w, scope.h, 0x0b0907, 0x4ade80);

    let mut status = Line::<64>::new();
    let _ = write!(status, "{rate} Hz  MIDI {midi_messages}  keys {}", organ.held_count());
    let status_x = header.x + header.w - layout.margin;
    canvas.text_right(status_x, header.y + layout.margin, status.as_str(), INK);
    let meter_w = header.w * 30 / 100;
    let meter_y = header.y + header.h - layout.margin - 4;
    canvas.fill_rect(status_x - meter_w, meter_y, meter_w as u32, 4, 0x0b0907);
    let fill = (meter_w as f32 * peak.min(1.0)) as u32;
    canvas.fill_rect(status_x - meter_w, meter_y, fill, 4, if peak > 0.95 { theme().red } else { theme().green });
}

fn draw_drawbars(canvas: &mut Canvas, layout: &Layout, organ: &Organ) {
    let panel = layout.drawbar_panel;
    canvas.fill_rect(panel.x, panel.y, panel.w as u32, panel.h as u32, PANEL);
    canvas.rect(panel.x, panel.y, panel.w as u32, panel.h as u32, WOOD_LIGHT);
    let knob_h = (panel.h / 20).max(10);
    for index in 0..DRAWBARS {
        let Some(column) = layout.widget_rect(Hit::Drawbar(index as u8)) else { continue };
        let position = organ.drawbars()[index];
        let centre = column.x + column.w / 2;
        let rail = (column.w / 10).max(3);
        canvas.fill_rect(centre - rail / 2, layout.track_top, rail as u32, (layout.track_bottom - layout.track_top) as u32, 0x0a0806);
        for tick in 0..=8u8 {
            let y = layout.knob_y(tick);
            canvas.fill_rect(centre - column.w * 3 / 10, y, (column.w * 6 / 10) as u32, 1, 0x4a3b2c);
        }
        let colour = bar_colour(index);
        let knob = layout.knob_y(position);
        let body = (column.w * 4 / 10).max(4);
        canvas.fill_rect(centre - body / 2, layout.track_top, body as u32, (knob - layout.track_top).max(0) as u32, colour);
        let knob_w = column.w * 7 / 10;
        canvas.fill_rect(centre - knob_w / 2, knob - knob_h / 2, knob_w as u32, knob_h as u32, colour);
        canvas.rect(centre - knob_w / 2, knob - knob_h / 2, knob_w as u32, knob_h as u32, if position > 0 { ACTIVE } else { 0x6b5a46 });
        let mut value = Line::<4>::new();
        let _ = write!(value, "{position}");
        center(canvas, Rect { x: column.x, y: panel.y + layout.margin / 2, w: column.w, h: GLYPH_H * layout.scale + layout.margin }, value.as_str(), layout.scale, if position > 0 { ACTIVE } else { INK });
        let footage = FOOTAGES[index];
        let short = footage.trim_end_matches('\'');
        let text = if gfx::text_width(footage) as i32 <= column.w { footage } else { short };
        let area = Rect { x: column.x, y: layout.track_bottom + layout.margin, w: column.w, h: GLYPH_H * layout.scale };
        center(canvas, area, text, 1, INK);
    }
}

fn draw_switches(canvas: &mut Canvas, layout: &Layout, organ: &Organ) {
    let panel = layout.switch_panel;
    canvas.fill_rect(panel.x, panel.y, panel.w as u32, panel.h as u32, PANEL);
    canvas.rect(panel.x, panel.y, panel.w as u32, panel.h as u32, WOOD_LIGHT);
    for &(text, x, y) in layout.labels.iter().filter(|(text, _, _)| !text.is_empty()) {
        canvas.text_scaled(x, y, text, layout.scale as u32, INK);
    }
    for widget in layout.widgets() {
        let rect = widget.rect;
        match widget.hit {
            Hit::Drawbar(_) | Hit::Key(_) => {}
            Hit::Slider(index) => {
                let value = slider_value(organ, index);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
                canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * value) as u32, rect.h as u32, 0x8a6200);
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, WOOD_LIGHT);
                let mut text = Line::<8>::new();
                let _ = write!(text, "{}%", (value * 100.0 + 0.5) as i32);
                center(canvas, rect, text.as_str(), layout.scale, INK);
            }
            hit => {
                // The two percussion toggles show their state in their words; a lit one is a state that is on.
                let on = match hit {
                    Hit::PercussionFast => organ.percussion_fast(),
                    Hit::PercussionSoft => organ.percussion_soft(),
                    other => lit(organ, other),
                };
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { ACTIVE } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, WOOD_LIGHT);
                center(canvas, rect, label(organ, hit), layout.scale, if on { 0x000000 } else { INK });
            }
        }
    }
}

fn draw_keyboard(canvas: &mut Canvas, layout: &Layout, organ: &Organ) {
    let notes = FIRST_KEY..FIRST_KEY + KEYS as u8;
    let black = |note: u8| matches!((note - FIRST_KEY) % 12, 1 | 3 | 6 | 8 | 10);
    for note in notes.clone().filter(|&n| !black(n)) {
        let rect = layout.key_rect(note);
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if organ.is_held(note) { ACTIVE } else { 0xf2ecd8 });
        canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, 0x000000);
        if (note - FIRST_KEY) % 12 == 0 && rect.w >= GLYPH_W * 2 {
            let mut name = Line::<4>::new();
            let _ = write!(name, "C{}", 2 + (note - FIRST_KEY) / 12);
            canvas.text(rect.x + 2, rect.y + rect.h - GLYPH_H - 2, name.as_str(), 0x555555);
        }
    }
    for note in notes.filter(|&n| black(n)) {
        let rect = layout.key_rect(note);
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if organ.is_held(note) { 0xc07800 } else { 0x141414 });
        canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, 0x000000);
    }
}

/// The organ as a touch target: drawbars, switches, sliders and the keyboard.
struct OrganTouch<'a> {
    layout: &'a Layout,
    organ: &'a mut Organ,
}

impl TouchTarget for OrganTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        press(self.layout, self.organ, hit, x, y);
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, x: i32, y: i32) -> Hit {
        let Hit::Key(old) = grab else {
            drag(self.layout, self.organ, grab, x, y);
            return grab;
        };
        // Sliding along the keyboard plays the keys on the way.
        if let Some(Hit::Key(new)) = self.layout.hit(x, y).filter(|h| matches!(h, Hit::Key(_))) {
            if new != old {
                self.organ.note_off(old);
                self.organ.note_on(new);
                return Hit::Key(new);
            }
        }
        grab
    }

    fn up(&mut self, grab: Hit) {
        if let Hit::Key(note) = grab {
            self.organ.note_off(note);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let start = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(start) => start,
        Err(reason) => {
            tape_rt::init(api);
            return println!("organ: {reason}.");
        }
    };
    let mut output = match Output::start(api, start.arguments) {
        Ok(output) => output,
        Err(reason) => return println!("organ: {}.", reason.as_str()),
    };
    let mut organ = Organ::new(output.rate);
    organ.set_reverb(0.25);

    let mut keys_shown = tape_rt::midi_devices() == 0;
    let mut layout = Layout::new(start.width as i32, start.height as i32, keys_shown);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(start.screen)) else {
        output.stop();
        return println!("organ: out of memory.");
    };
    if !screen.open() {
        output.stop();
        return println!("organ: cannot take the screen.");
    }
    println!(
        "organ: {} Hz, {} channel(s) of {}-bit {}, outputs {}-{}, {} ms queued. Escape quits.",
        output.rate,
        output.channels,
        output.format.bits,
        if output.format.float { "float" } else { "integer" },
        output.first + 1,
        output.first + 2,
        output.latency_ms
    );
    output.prime(|frames| organ.render(frames));

    let mut decoder = KeyDecoder::new();
    let mut touches = Touches::new(layout.width, layout.height, start.screen);
    let mut typed = TypedKeys::new(TYPED_US);
    let mut typed_base = 48u8;
    let mut midi_messages = 0u32;
    let mut next_animation = 0u64;
    let mut next_check = 0u64;
    loop {
        let mut changed = false;
        let keep_going = poll_keys(api, &mut decoder, |key| match key {
            Key::Char(c) => {
                if let Some(note) = note_for_char(c, typed_base) {
                    organ.note_on(note);
                    typed.press(note, tape_rt::micros());
                    changed = true;
                }
            }
            Key::Left => typed_base = typed_base.saturating_sub(12).max(FIRST_KEY),
            Key::Right => typed_base = (typed_base + 12).min(FIRST_KEY + 36),
            Key::Tab => {
                organ.set_leslie(if organ.leslie() == Leslie::Fast { Leslie::Slow } else { Leslie::Fast });
                changed = true;
            }
            _ => {}
        });
        if !keep_going {
            break;
        }
        let now = tape_rt::micros();
        changed |= typed.expire(now, |note| organ.note_off(note));
        changed |= touches.poll(&mut OrganTouch { layout: &layout, organ: &mut organ });
        if now >= next_check {
            // A MIDI keyboard makes the on-screen one redundant; it comes back when the last device is unplugged.
            next_check = now + 500_000;
            let want_keys = tape_rt::midi_devices() == 0;
            if want_keys != keys_shown {
                keys_shown = want_keys;
                layout = Layout::new(start.width as i32, start.height as i32, keys_shown);
                changed = true;
            }
        }
        let accepted = poll_midi(|message| organ.midi(message));
        midi_messages += accepted;
        changed |= accepted > 0;

        output.pump(|frames| organ.render(frames));
        if (organ.held_count() > 0 || output.peak > 0.002) && now >= next_animation {
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
                draw_header(&mut canvas, &layout, &organ, &output, midi_messages);
                draw_drawbars(&mut canvas, &layout, &organ);
                draw_switches(&mut canvas, &layout, &organ);
                if layout.keys_shown {
                    draw_keyboard(&mut canvas, &layout, &organ);
                }
            }
            blit_banded(&screen, || {
                output.pump(|frames| organ.render(frames));
            });
        }
        tape_rt::idle();
    }
    organ.all_notes_off();
    output.stop();
    screen.close();
    println!("organ: stopped.");
}
