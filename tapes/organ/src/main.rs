//! `organ`: a Hammond-style drawbar organ for the touch screen. Nine drawbars, presets, percussion, scanner vibrato/chorus, a
//! Leslie and overdrive are on the screen and answer to a finger; the 61 keys (C2-C7) answer to a USB-MIDI keyboard, to the
//! on-screen keyboard and to the typed-key keyboard. The sound is `crates/organ`; this file runs it: MIDI in (ABI 25), touch in
//! (ABI 24), stereo audio out, and the picture. The picture is sent in bands with the audio pumped in between, because a
//! full-screen framebuffer write takes longer than the audio buffer holds.
#![no_std]
#![no_main]

use audio_sample::Format;
use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gfx::theme::theme;
use organ::{DRAWBARS, FIRST_KEY, FOOTAGES, KEYS, Leslie, Organ};
use organ_tape::{GLYPH_H, GLYPH_W, Hit, Layout, Rect, drag, label, lit, note_for_char, press, slider_value};
use tape_rt::{AudioInfo, Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"organ [device=N] [outdev=N] [screen=N] [rate=HZ] [latency=MS]
A drawbar organ in the Hammond B-3 mould. The screen has nine drawbars (drag
them up and down), six presets, percussion (2nd/3rd harmonic, fast/slow,
soft/normal), scanner vibrato V1-V3 and chorus C1-C3, a Leslie (Off, Stop,
Slow, Fast: the rotors speed up and slow down like the real ones), overdrive,
volume, key click, and a 61-key keyboard that a finger plays. A USB-MIDI
keyboard plays it too (note on/off; controller 123 stops all notes). The sound
is stereo, 32-bit float inside, whatever the audio device takes (24 bit, 48 kHz
USB audio, say). latency=MS is how much audio is kept queued (10-200, default
40): lower is quicker to respond, higher survives a slow screen.
Keys: z s x d c v g b h n j m , and q 2 w 3 e r 5 t 6 y 7 u i play notes
(each rings 0.3 s, a terminal sends no key release); Left/Right move them an
octave; Tab flips the Leslie between Slow and Fast; Escape quits.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const CHUNK: usize = 256;
const SCOPE_LEN: usize = 1024;
const BAND_ROWS: u32 = 32;
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

static mut ORGAN: Option<Organ> = None;
static mut FRAMES: [[f32; 2]; CHUNK] = [[0.0; 2]; CHUNK];
static mut PACKED: [u8; CHUNK * 8 * 4] = [0; CHUNK * 8 * 4];
static mut SCOPE: [f32; SCOPE_LEN] = [0.0; SCOPE_LEN];
static mut SCOPE_AT: usize = 0;

fn organ() -> &'static mut Organ {
    unsafe { (*core::ptr::addr_of_mut!(ORGAN)).as_mut().expect("the organ is made before the loop") }
}

/// The playback side: how the device wants its frames, and how many to keep queued.
struct Audio {
    channels: usize,
    format: Format,
    capacity: u32,
    target: usize,
    peak: f32,
}

/// Tops the device's queue up to `target` frames with freshly rendered sound. True when it wrote something.
fn pump(api: &TapeApi, audio: &mut Audio) -> bool {
    let mut info = AudioInfo::default();
    (api.audio_info)(&mut info);
    audio.capacity = audio.capacity.max(info.playback_space);
    let slot = audio.format.slot;
    let frame_bytes = audio.channels * slot;
    let queued = (audio.capacity - info.playback_space) as usize / frame_bytes;
    if queued >= audio.target {
        return false;
    }
    let frames = (audio.target - queued).min(CHUNK);
    let (rendered, packed, scope) = unsafe { (&mut (&mut *core::ptr::addr_of_mut!(FRAMES))[..frames], &mut *core::ptr::addr_of_mut!(PACKED), &mut *core::ptr::addr_of_mut!(SCOPE)) };
    organ().render(rendered);
    let mut peak = audio.peak * 0.98;
    for (n, frame) in rendered.iter().enumerate() {
        for channel in 0..audio.channels {
            let value = match channel {
                0 if audio.channels == 1 => 0.5 * (frame[0] + frame[1]),
                0 => frame[0],
                1 => frame[1],
                _ => 0.0,
            };
            let at = n * frame_bytes + channel * slot;
            audio.format.write(value, &mut packed[at..at + slot]);
        }
        peak = peak.max(frame[0].abs()).max(frame[1].abs());
        unsafe {
            scope[SCOPE_AT] = frame[0];
            SCOPE_AT = (SCOPE_AT + 1) % SCOPE_LEN;
        }
    }
    audio.peak = peak;
    (api.audio_write)(packed.as_ptr(), frames * frame_bytes);
    true
}

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

fn draw_header(canvas: &mut Canvas, layout: &Layout, organ: &Organ, rate: u32, midi_messages: u32, peak: f32) {
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, WOOD_LIGHT);
    let scale = layout.scale as u32;
    canvas.text_scaled(header.x + layout.margin, header.y + (header.h - GLYPH_H * layout.scale) / 2, "FOSFOR ORGAN", scale, ACTIVE);

    // The sound itself: the last samples, started at a rising zero crossing so a steady note stands still.
    let scope = Rect { x: header.x + header.w * 28 / 100, y: header.y + 3, w: header.w * 36 / 100, h: header.h - 6 };
    canvas.fill_rect(scope.x, scope.y, scope.w as u32, scope.h as u32, 0x0b0907);
    let samples = unsafe { &*core::ptr::addr_of!(SCOPE) };
    let newest = unsafe { SCOPE_AT };
    let at = |n: usize| samples[(newest + n) % SCOPE_LEN];
    let start = (0..SCOPE_LEN / 2).find(|&n| at(n) <= 0.0 && at(n + 1) > 0.0).unwrap_or(0);
    let points = (SCOPE_LEN / 2).min(scope.w as usize);
    let mid = scope.y + scope.h / 2;
    let point = |n: usize| (scope.x + (n as i32 * scope.w) / points as i32, mid - (at(start + n * (SCOPE_LEN / 2) / points) * (scope.h as f32 * 0.45)) as i32);
    let mut last = point(0);
    for n in 1..points {
        let next = point(n);
        canvas.line(last.0, last.1, next.0, next.1, 0x4ade80);
        last = next;
    }

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

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let start = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(start) => start,
        Err(reason) => {
            tape_rt::init(api);
            return println!("organ: {reason}.");
        }
    };
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.playback_channels == 0 {
        return println!("organ: no audio output (USB audio or HD Audio with a playback side).");
    }
    let wanted = tape_rt::arg_value(start.arguments, "rate").and_then(|value| value.parse::<u32>().ok());
    let offered = &info.rates[..(info.rate_count as usize).min(8)];
    let rate = wanted.or_else(|| offered.contains(&48000).then_some(48000)).or_else(|| offered.first().copied()).unwrap_or(48000);
    let latency_ms = tape_rt::arg_value(start.arguments, "latency").and_then(|value| value.parse::<u32>().ok()).unwrap_or(40).clamp(10, 200);
    if (api.audio_start)(rate, 0, 1) != 0 {
        return println!("organ: the audio device does not start at {rate} Hz.");
    }
    (api.audio_info)(&mut info);
    let format = Format::new(info.playback_slot_bytes as usize, info.playback_bits as usize, tape_rt::audio_float_of(api).1);
    let channels = info.playback_channels as usize;
    if !format.valid() || channels == 0 || channels * format.slot > 32 {
        (api.audio_stop)();
        return println!("organ: cannot write this device's sample format ({} byte slots, {} channels).", format.slot, channels);
    }
    let mut audio = Audio { channels, format, capacity: info.playback_space, target: (rate * latency_ms / 1000) as usize, peak: 0.0 };
    unsafe { *core::ptr::addr_of_mut!(ORGAN) = Some(Organ::new(rate)) };

    let layout = Layout::new(start.width as i32, start.height as i32);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(start.screen)) else {
        (api.audio_stop)();
        return println!("organ: out of memory.");
    };
    if !screen.open() {
        (api.audio_stop)();
        return println!("organ: cannot take the screen.");
    }
    println!("organ: {} Hz, {} channel(s) of {}-bit {}, {} ms queued. Escape quits.", rate, channels, info.playback_bits, if format.float { "float" } else { "integer" }, latency_ms);
    // Fill the queue before the first picture: the first draw is the slowest.
    while pump(api, &mut audio) {}

    let mut decoder = KeyDecoder::new();
    let mut grabs: [Option<(i32, Hit)>; 8] = [None; 8];
    let mut typed: [Option<(u8, u64)>; 8] = [None; 8];
    let mut typed_base = 48u8;
    let mut midi_messages = 0u32;
    let mut next_animation = 0u64;
    loop {
        let mut changed = false;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            let now = tape_rt::micros();
            match key {
                Key::Char(c) => {
                    if let Some(note) = note_for_char(c, typed_base) {
                        organ().note_on(note);
                        let slot = typed.iter().position(|t| t.is_some_and(|(n, _)| n == note)).or_else(|| typed.iter().position(|t| t.is_none()));
                        if let Some(slot) = slot {
                            typed[slot] = Some((note, now + TYPED_US));
                        }
                        changed = true;
                    }
                }
                Key::Left => typed_base = typed_base.saturating_sub(12).max(FIRST_KEY),
                Key::Right => typed_base = (typed_base + 12).min(FIRST_KEY + 36),
                Key::Tab => {
                    organ().set_leslie(if organ().leslie() == Leslie::Fast { Leslie::Slow } else { Leslie::Fast });
                    changed = true;
                }
                _ => {}
            }
        });
        if !keep_going {
            break;
        }
        let now = tape_rt::micros();
        for slot in typed.iter_mut() {
            if let Some((note, until)) = *slot {
                if now >= until {
                    organ().note_off(note);
                    *slot = None;
                    changed = true;
                }
            }
        }

        while let Some(touch) = tape_rt::touch_event() {
            if touch.screen >= 0 && touch.screen != start.screen as i32 {
                continue; // a panel on another screen
            }
            let Some((max_x, max_y)) = tape_rt::touch_extent(touch.panel) else { continue };
            let (x, y) = (touch.x * layout.width / max_x.max(1), touch.y * layout.height / max_y.max(1));
            let held = grabs.iter().position(|g| g.is_some_and(|(id, _)| id == touch.id));
            changed = true;
            match (touch.down, held) {
                (true, None) => {
                    if let (Some(hit), Some(free)) = (layout.hit(x, y), grabs.iter().position(|g| g.is_none())) {
                        press(&layout, organ(), hit, x, y);
                        grabs[free] = Some((touch.id, hit));
                    }
                }
                (true, Some(slot)) => {
                    let Some((id, hit)) = grabs[slot] else { continue };
                    if let Hit::Key(old) = hit {
                        // Sliding along the keyboard plays the keys on the way.
                        if let Some(Hit::Key(new)) = layout.hit(x, y).filter(|h| matches!(h, Hit::Key(_))) {
                            if new != old {
                                organ().note_off(old);
                                organ().note_on(new);
                                grabs[slot] = Some((id, Hit::Key(new)));
                            }
                        }
                    } else {
                        drag(&layout, organ(), hit, x, y);
                    }
                }
                (false, Some(slot)) => {
                    if let Some((_, Hit::Key(note))) = grabs[slot] {
                        organ().note_off(note);
                    }
                    grabs[slot] = None;
                }
                (false, None) => {}
            }
        }

        for _ in 0..64 {
            let Some(message) = tape_rt::midi_event() else { break };
            if organ().midi(&message.data[..message.len]) {
                midi_messages += 1;
                changed = true;
            }
        }

        pump(api, &mut audio);
        if organ().held_count() > 0 || audio.peak > 0.002 {
            if now >= next_animation {
                next_animation = now + 60_000;
                changed = true;
            }
        }
        if changed {
            screen.mark_dirty();
        }
        if screen.due() {
            {
                let mut canvas = screen.frame();
                canvas.clear(BACKGROUND);
                draw_header(&mut canvas, &layout, organ(), rate, midi_messages, audio.peak);
                draw_drawbars(&mut canvas, &layout, organ());
                draw_switches(&mut canvas, &layout, organ());
                draw_keyboard(&mut canvas, &layout, organ());
            }
            let width = screen.width;
            let mut row = 0;
            while row < screen.height {
                let rows = BAND_ROWS.min(screen.height - row);
                let pixels = &screen.pixels()[(row * width) as usize..((row + rows) * width) as usize];
                tape_rt::gfx_blit_on(screen.index, 0, row, width, rows, pixels, width);
                pump(api, &mut audio);
                row += rows;
            }
        }
        tape_rt::idle();
    }
    organ().all_notes_off();
    (api.audio_stop)();
    screen.close();
    println!("organ: stopped.");
}
