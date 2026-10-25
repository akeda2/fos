//! `epiano`: a Rhodes-style electric piano for the touch screen and a USB-MIDI keyboard, built for playing live: the audio
//! queue defaults to 10 ms and MIDI is polled right before every top-up and again between the bands of a screen blit, so a
//! note-on never waits for a frame. The sound is `crates/epiano`; this file runs it, like `strings` does.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gfx::theme::theme;
use instrument_rt::{FIRST_KEY, Output, Rect, TouchTarget, Touches, TypedKeys, blit_banded, note_for_char, poll_midi};
use epiano::EPiano;
use epiano_tape::{GLYPH_H, Hit, Layout, SLIDERS, drag, label, lit, press, slider_value};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"epiano [device=N] [outdev=N] [screen=N] [rate=HZ] [latency=MS] [out=N]
A Rhodes-style electric piano for playing live. Each note is a struck tine
(fundamental plus a bell and a ting that a hard hit brings out) with a hammer
thump, through its own pickup (Bark: a hard hit barks, a soft one stays
sweet; Touch sets how hard is hard, Soft raises the velocity floor for a
keyboard that plays loud when touched lightly; the header shows "vel") and a
vowel filter: two formants that a hard hit opens towards "a" and that close
as the note dies (Vowel sets where it rests, Glide how far it moves, Q how
sharp it is, Range how high it opens: low is a sung "a", high a wah's
squeal; the Vowel button off gives the older pitch sweep instead). Then a
tube overdrive (Drive), a chorus, a tremolo auto-pan, a phaser, the tape
echo (Echo, Repeat, Feedback) and a reverb.
A USB-MIDI keyboard plays it, 88 keys (note on/off with velocity, sustain
pedal, controller 123 stops all notes, an expression pedal (CC 11) is a wah
pedal, as is the Wah slider). The pitch wheel runs the tape echo's tape
faster or slower (the repeats swoop); the mod wheel (CC 1) adds tremolo
depth, echo feedback and/or bark, or works the wah, as the Trem / Echo fb /
Bark / All / Wah buttons say. The Bar / Tine / FM / Hybrid / Metal / Chime
buttons (or MIDI program change 0-5) pick how the tine is made, for
comparing by ear. The on-screen keyboard (hidden while a MIDI keyboard is
connected) strikes harder the lower on the key you touch. Pedal on the screen holds notes. The default
latency is 10 ms (latency=MS, 10-200): raise it if the sound crackles. out=N
picks the output pair on a multichannel card: N is the left channel (out=9
plays on outputs 9 and 10; default 1).
Keys: z s x d c v g b h n j m , and q 2 w 3 e r 5 t 6 y 7 u i play notes
(each rings 0.3 s, a terminal sends no key release); Left/Right move them an
octave; Tab flips the pedal; Escape quits.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
/// How long a typed key sounds.
const TYPED_US: u64 = 300_000;
/// A terminal key has no velocity.
const TYPED_VELOCITY: u8 = 90;
/// The audio queue a live instrument can afford (the usual is 40 ms).
const LATENCY_MS: u32 = 10;
const BACKGROUND: u32 = 0x0e1218;
const PANEL: u32 = 0x1a212b;
const EDGE: u32 = 0x4a5a70;
const ACTIVE: u32 = 0x5ec8ff;
const BUTTON_OFF: u32 = 0x2a3340;
const INK: u32 = 0xd8e4f0;

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, piano: &EPiano, output: &Output, midi_messages: u32) {
    let (rate, peak) = (output.rate, output.peak);
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, EDGE);
    let scale = layout.scale as u32;
    canvas.text_scaled(header.x + layout.margin, header.y + (header.h - GLYPH_H * layout.scale) / 2, "FOSFOR EPIANO", scale, ACTIVE);
    let scope = Rect { x: header.x + header.w * 30 / 100, y: header.y + 3, w: header.w * 34 / 100, h: header.h - 6 };
    output.draw_scope(canvas, scope.x, scope.y, scope.w, scope.h, 0x080b0f, 0x4ade80);
    let mut status = Line::<64>::new();
    let _ = write!(status, "{rate} Hz  MIDI {midi_messages}  vel {}  keys {}  bend {:+}%", piano.last_velocity(), piano.held_count(), (piano.pitch_bend() * 100.0) as i32);
    let status_x = header.x + header.w - layout.margin;
    canvas.text_right(status_x, header.y + layout.margin, status.as_str(), INK);
    let meter_w = header.w * 30 / 100;
    let meter_y = header.y + header.h - layout.margin - 4;
    canvas.fill_rect(status_x - meter_w, meter_y, meter_w as u32, 4, 0x080b0f);
    canvas.fill_rect(status_x - meter_w, meter_y, (meter_w as f32 * peak.min(1.0)) as u32, 4, if peak > 0.95 { theme().red } else { theme().green });
}

fn draw_controls(canvas: &mut Canvas, layout: &Layout, piano: &EPiano) {
    let panel = layout.panel;
    canvas.fill_rect(panel.x, panel.y, panel.w as u32, panel.h as u32, PANEL);
    canvas.rect(panel.x, panel.y, panel.w as u32, panel.h as u32, EDGE);
    for widget in layout.widgets() {
        let rect = widget.rect;
        match widget.hit {
            Hit::Key(_) => {}
            Hit::Slider(index) => {
                let value = slider_value(piano, index);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
                canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * value) as u32, rect.h as u32, 0x1f5f80);
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                let mut text = Line::<32>::new();
                let _ = write!(text, "{} {}%", SLIDERS[index as usize], (value * 100.0 + 0.5) as i32);
                center(canvas, rect, text.as_str(), layout.scale, INK);
            }
            hit => {
                let on = lit(piano, hit);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { ACTIVE } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                center(canvas, rect, label(hit), layout.scale, if on { 0x000000 } else { INK });
            }
        }
    }
}

/// The piano as a touch target: sliders, switches and the keyboard.
struct EPianoTouch<'a> {
    layout: &'a Layout,
    piano: &'a mut EPiano,
}

impl TouchTarget for EPianoTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        press(self.layout, self.piano, hit, x, y);
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, x: i32, y: i32) -> Hit {
        let Hit::Key(old) = grab else {
            drag(self.layout, self.piano, grab, x, y);
            return grab;
        };
        // Sliding along the keyboard plays the keys on the way.
        if let Some(Hit::Key(new)) = self.layout.hit(x, y).filter(|h| matches!(h, Hit::Key(_))) {
            if new != old {
                self.piano.note_off(old);
                self.piano.note_on(new, self.layout.velocity_at(new, y));
                return Hit::Key(new);
            }
        }
        grab
    }

    fn up(&mut self, grab: Hit) {
        if let Hit::Key(note) = grab {
            self.piano.note_off(note);
        }
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let start = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(start) => start,
        Err(reason) => {
            tape_rt::init(api);
            return println!("epiano: {reason}.");
        }
    };
    let mut output = match Output::start_with_latency(api, start.arguments, LATENCY_MS) {
        Ok(output) => output,
        Err(reason) => return println!("epiano: {}.", reason.as_str()),
    };
    let mut piano = EPiano::new(output.rate);

    let mut keys_shown = tape_rt::midi_devices() == 0;
    let mut layout = Layout::new(start.width as i32, start.height as i32, keys_shown);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(start.screen)) else {
        output.stop();
        return println!("epiano: out of memory.");
    };
    if !screen.open() {
        output.stop();
        return println!("epiano: cannot take the screen.");
    }
    println!(
        "epiano: {} Hz, {} channel(s) of {}-bit {}, outputs {}-{}, {} ms queued. Escape quits.",
        output.rate,
        output.channels,
        output.format.bits,
        if output.format.float { "float" } else { "integer" },
        output.first + 1,
        output.first + 2,
        output.latency_ms
    );
    output.prime(|frames| piano.render(frames));

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
                    piano.note_on(note, TYPED_VELOCITY);
                    typed.press(note, tape_rt::micros());
                    changed = true;
                }
            }
            Key::Left => typed_base = typed_base.saturating_sub(12).max(FIRST_KEY),
            Key::Right => typed_base = (typed_base + 12).min(FIRST_KEY + 36),
            Key::Tab => {
                piano.set_sustain(!piano.sustain());
                changed = true;
            }
            _ => {}
        });
        if !keep_going {
            break;
        }
        let now = tape_rt::micros();
        changed |= typed.expire(now, |note| piano.note_off(note));
        changed |= touches.poll(&mut EPianoTouch { layout: &layout, piano: &mut piano });
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
        // MIDI is read right before the audio top-up, so a note that has arrived is in the very next block.
        let accepted = poll_midi(|message| piano.midi(message));
        midi_messages += accepted;
        changed |= accepted > 0;

        output.pump(|frames| piano.render(frames));
        if (piano.held_count() > 0 || output.peak > 0.002) && now >= next_animation {
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
                draw_header(&mut canvas, &layout, &piano, &output, midi_messages);
                draw_controls(&mut canvas, &layout, &piano);
                if layout.keys_shown {
                    layout.keyboard.draw(&mut canvas, |note| piano.is_held(note), ACTIVE, 0x2a90c8);
                }
            }
            // Between bands the same two steps, MIDI first: a blit takes long enough to empty a 10 ms queue.
            blit_banded(&screen, || {
                midi_messages += poll_midi(|message| piano.midi(message));
                output.pump(|frames| piano.render(frames));
            });
        }
        tape_rt::idle();
    }
    piano.all_notes_off();
    output.stop();
    screen.close();
    println!("epiano: stopped.");
}
