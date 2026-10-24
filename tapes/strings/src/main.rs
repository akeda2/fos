//! `strings`: a divide-down string machine in the Solina mould for the touch screen. Six sections (Violin, Viola, Cello, Bass,
//! Brass, Piano) with a fader each, attack and release, an ensemble chorus, a phaser and reverb are on the screen; the 61 keys
//! (C2-C7) answer to a USB-MIDI keyboard (mod wheel, sustain pedal), to the on-screen keyboard and to the typed-key keyboard.
//! The sound is `crates/strings`; this file runs it, like `organ` does: MIDI in, touch in, stereo audio out, and the picture
//! sent in bands with the audio pumped in between.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gfx::theme::theme;
use instrument_rt::{FIRST_KEY, Output, Rect, TouchTarget, Touches, TypedKeys, blit_banded, note_for_char, poll_midi};
use strings::{SECTION_NAMES, SECTIONS, Strings};
use strings_tape::{GLYPH_H, Hit, Layout, SLIDERS, drag, label, lit, press, slider_value};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"strings [device=N] [outdev=N] [screen=N] [rate=HZ] [latency=MS] [out=N]
A divide-down string machine in the Solina mould. Six sections, each with a
fader on the screen: Violin, Viola, Cello, Bass, Brass (a slow swell) and
Piano (it decays while held). Attack and Release shape the strings; Ensemble
is the three-voice chorus, Phaser a six-stage phaser, Reverb a room. A
USB-MIDI keyboard plays it (note on/off, sustain pedal, controller 123 stops
all notes); the mod wheel sweeps the phaser speed and/or the ensemble depth,
as the Phaser / Ens / Both buttons say. Pedal on the screen holds notes like
the sustain pedal. While a MIDI keyboard is connected the on-screen keyboard
is hidden. The sound is stereo, 32-bit float inside, whatever the audio device
takes. latency=MS is how much audio is kept queued (10-200, default 40).
out=N picks the output pair on a multichannel card: N is the left channel
(out=9 plays on outputs 9 and 10; default 1).
Keys: z s x d c v g b h n j m , and q 2 w 3 e r 5 t 6 y 7 u i play notes
(each rings 0.3 s, a terminal sends no key release); Left/Right move them an
octave; Tab flips the pedal; Escape quits.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
/// How long a typed key sounds.
const TYPED_US: u64 = 300_000;
const BACKGROUND: u32 = 0x0e1218;
const PANEL: u32 = 0x1a212b;
const EDGE: u32 = 0x4a5a70;
const ACTIVE: u32 = 0x5ec8ff;
const BUTTON_OFF: u32 = 0x2a3340;
const INK: u32 = 0xd8e4f0;
const FADER_COLOURS: [u32; SECTIONS] = [0xe8d070, 0xd09050, 0xb06040, 0x8a4a8a, 0xd8d8d8, 0x70b070];

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, machine: &Strings, output: &Output, midi_messages: u32) {
    let (rate, peak) = (output.rate, output.peak);
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, EDGE);
    let scale = layout.scale as u32;
    canvas.text_scaled(header.x + layout.margin, header.y + (header.h - GLYPH_H * layout.scale) / 2, "FOSFOR STRINGS", scale, ACTIVE);
    let scope = Rect { x: header.x + header.w * 30 / 100, y: header.y + 3, w: header.w * 34 / 100, h: header.h - 6 };
    output.draw_scope(canvas, scope.x, scope.y, scope.w, scope.h, 0x080b0f, 0x4ade80);
    let mut status = Line::<64>::new();
    let _ = write!(status, "{rate} Hz  MIDI {midi_messages}  keys {}", machine.held_count());
    let status_x = header.x + header.w - layout.margin;
    canvas.text_right(status_x, header.y + layout.margin, status.as_str(), INK);
    let meter_w = header.w * 30 / 100;
    let meter_y = header.y + header.h - layout.margin - 4;
    canvas.fill_rect(status_x - meter_w, meter_y, meter_w as u32, 4, 0x080b0f);
    canvas.fill_rect(status_x - meter_w, meter_y, (meter_w as f32 * peak.min(1.0)) as u32, 4, if peak > 0.95 { theme().red } else { theme().green });
}

fn draw_faders(canvas: &mut Canvas, layout: &Layout, machine: &Strings) {
    let panel = layout.fader_panel;
    canvas.fill_rect(panel.x, panel.y, panel.w as u32, panel.h as u32, PANEL);
    canvas.rect(panel.x, panel.y, panel.w as u32, panel.h as u32, EDGE);
    let knob_h = (panel.h / 14).max(10);
    for index in 0..SECTIONS {
        let Some(column) = layout.widget_rect(Hit::Fader(index as u8)) else { continue };
        let level = machine.level(index);
        let centre = column.x + column.w / 2;
        let rail = (column.w / 12).max(3);
        canvas.fill_rect(centre - rail / 2, layout.track_top, rail as u32, (layout.track_bottom - layout.track_top) as u32, 0x080b0f);
        let knob = layout.knob_y(level);
        let colour = FADER_COLOURS[index];
        let body = (column.w * 3 / 10).max(4);
        canvas.fill_rect(centre - body / 2, knob, body as u32, (layout.track_bottom - knob).max(0) as u32, colour);
        let knob_w = column.w * 7 / 10;
        canvas.fill_rect(centre - knob_w / 2, knob - knob_h / 2, knob_w as u32, knob_h as u32, colour);
        canvas.rect(centre - knob_w / 2, knob - knob_h / 2, knob_w as u32, knob_h as u32, if level > 0.0 { ACTIVE } else { EDGE });
        let mut value = Line::<8>::new();
        let _ = write!(value, "{}", (level * 100.0 + 0.5) as i32);
        let top = Rect { x: column.x, y: panel.y + layout.margin, w: column.w, h: GLYPH_H * layout.scale };
        center(canvas, top, value.as_str(), layout.scale, if level > 0.0 { ACTIVE } else { INK });
        let name = Rect { x: column.x, y: layout.track_bottom + layout.margin * 2, w: column.w, h: GLYPH_H * layout.scale };
        center(canvas, name, SECTION_NAMES[index], if gfx::text_width(SECTION_NAMES[index]) as i32 * layout.scale <= column.w { layout.scale } else { 1 }, INK);
    }
}

fn draw_controls(canvas: &mut Canvas, layout: &Layout, machine: &Strings) {
    let panel = layout.control_panel;
    canvas.fill_rect(panel.x, panel.y, panel.w as u32, panel.h as u32, PANEL);
    canvas.rect(panel.x, panel.y, panel.w as u32, panel.h as u32, EDGE);
    for widget in layout.widgets() {
        let rect = widget.rect;
        match widget.hit {
            Hit::Fader(_) | Hit::Key(_) => {}
            Hit::Slider(index) => {
                let value = slider_value(machine, index);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
                canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * value) as u32, rect.h as u32, 0x1f5f80);
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                let mut text = Line::<32>::new();
                let _ = write!(text, "{} {}%", SLIDERS[index as usize], (value * 100.0 + 0.5) as i32);
                center(canvas, rect, text.as_str(), layout.scale, INK);
            }
            hit => {
                let on = lit(machine, hit);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { ACTIVE } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                center(canvas, rect, label(hit), layout.scale, if on { 0x000000 } else { INK });
            }
        }
    }
}

/// The machine as a touch target: faders, sliders, switches and the keyboard.
struct StringsTouch<'a> {
    layout: &'a Layout,
    machine: &'a mut Strings,
}

impl TouchTarget for StringsTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        press(self.layout, self.machine, hit, x, y);
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, x: i32, y: i32) -> Hit {
        let Hit::Key(old) = grab else {
            drag(self.layout, self.machine, grab, x, y);
            return grab;
        };
        // Sliding along the keyboard plays the keys on the way.
        if let Some(Hit::Key(new)) = self.layout.hit(x, y).filter(|h| matches!(h, Hit::Key(_))) {
            if new != old {
                self.machine.note_off(old);
                self.machine.note_on(new);
                return Hit::Key(new);
            }
        }
        grab
    }

    fn up(&mut self, grab: Hit) {
        if let Hit::Key(note) = grab {
            self.machine.note_off(note);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let start = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(start) => start,
        Err(reason) => {
            tape_rt::init(api);
            return println!("strings: {reason}.");
        }
    };
    let mut output = match Output::start(api, start.arguments) {
        Ok(output) => output,
        Err(reason) => return println!("strings: {}.", reason.as_str()),
    };
    let mut machine = Strings::new(output.rate);

    let mut keys_shown = tape_rt::midi_devices() == 0;
    let mut layout = Layout::new(start.width as i32, start.height as i32, keys_shown);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(start.screen)) else {
        output.stop();
        return println!("strings: out of memory.");
    };
    if !screen.open() {
        output.stop();
        return println!("strings: cannot take the screen.");
    }
    println!(
        "strings: {} Hz, {} channel(s) of {}-bit {}, outputs {}-{}, {} ms queued. Escape quits.",
        output.rate,
        output.channels,
        output.format.bits,
        if output.format.float { "float" } else { "integer" },
        output.first + 1,
        output.first + 2,
        output.latency_ms
    );
    output.prime(|frames| machine.render(frames));

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
                    machine.note_on(note);
                    typed.press(note, tape_rt::micros());
                    changed = true;
                }
            }
            Key::Left => typed_base = typed_base.saturating_sub(12).max(FIRST_KEY),
            Key::Right => typed_base = (typed_base + 12).min(FIRST_KEY + 36),
            Key::Tab => {
                machine.set_sustain(!machine.sustain());
                changed = true;
            }
            _ => {}
        });
        if !keep_going {
            break;
        }
        let now = tape_rt::micros();
        changed |= typed.expire(now, |note| machine.note_off(note));
        changed |= touches.poll(&mut StringsTouch { layout: &layout, machine: &mut machine });
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
        let accepted = poll_midi(|message| machine.midi(message));
        midi_messages += accepted;
        changed |= accepted > 0;

        output.pump(|frames| machine.render(frames));
        if (machine.held_count() > 0 || output.peak > 0.002) && now >= next_animation {
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
                draw_header(&mut canvas, &layout, &machine, &output, midi_messages);
                draw_faders(&mut canvas, &layout, &machine);
                draw_controls(&mut canvas, &layout, &machine);
                if layout.keys_shown {
                    layout.keyboard.draw(&mut canvas, |note| machine.is_held(note), ACTIVE, 0x2a90c8);
                }
            }
            blit_banded(&screen, || {
                output.pump(|frames| machine.render(frames));
            });
        }
        tape_rt::idle();
    }
    machine.all_notes_off();
    output.stop();
    screen.close();
    println!("strings: stopped.");
}
