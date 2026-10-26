//! `phono`: a phono equaliser. A flat (unequalised) cartridge or preamp signal comes in on the audio device, goes through the
//! chosen record equalisation curve (microgroove or 78 rpm), an optional rumble filter, an adjustable bass boost, mono and
//! polarity switches, and goes out again. The sound is `crates/phono`; this file runs it, headless (`service`) or with the
//! curves and switches on the touch screen.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gfx::theme::theme;
use instrument_rt::{Output, Rect, TouchTarget, Touches, blit_banded};
use phono::{CURVES, Phono, curve_db, curves::Groove};
use phono_tape::{DB_MAX, DB_MIN, GLYPH_H, Hit, Layout, Setup, TAPE_NAME, drag, label, lit, press, slider_text, slider_value};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"phono [service] [@FILE] [curve=ID] [mono=on|off] [polarity=P] [rumble=HZ]
[bass=DB] [bassfreq=HZ] [bassq=Q] [trim=DB]
[device=N|loopN] [outdev=N|loopN] [screen=N]
[rate=HZ] [latency=MS] [in=N] [out=N]
A phono equaliser: a flat cartridge or preamp signal on the audio input comes
out equalised. Without `service` the touch screen shows the response curve,
every curve as a button, the bass boost, rumble and trim sliders, Mono, the
polarity switches and Save. `service` runs without a screen: it prints one
line, stays quiet and runs until Escape (or from the start file).
Settings come from phono.cfg (or @FILE), then the words on the command line.
Save (screen, or key w) writes phono.cfg: the file `service` reads.
Curves: riaa iec columbia-lp nab nab-1942 aes london-lp teldec rca-lp rca-45
iec-78 ccir-78 decca-78 columbia-78 rca-78 euro-78 blumlein-300 flat-500
american-78-6 american-78-8 flat. polarity= normal, left, right or both.
rumble=off or 10-100 Hz (steep high-pass). bass= 0-12 dB boost at bassfreq=
60-150 Hz with width bassq= 0.3-3. trim= -24 to +12 dB.
Keys: m mono, l / r invert, [ ] previous / next curve, w save; Escape quits.
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const BACKGROUND: u32 = 0x0c1014;
const PANEL: u32 = 0x1a2128;
const EDGE: u32 = 0x4a5a68;
const ACTIVE: u32 = 0xffb347;
const BUTTON_OFF: u32 = 0x2a343e;
const INK: u32 = 0xe4ecf0;
const BAR: u32 = 0x1c6a8a;
const GRID: u32 = 0x26323c;
const REFERENCE: u32 = 0x5a6e7c;
const LIVE: u32 = 0x4ade80;

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn meter(canvas: &mut Canvas, x: i32, y: i32, w: i32, level: f32) {
    canvas.fill_rect(x, y, w as u32, 4, 0x080b0f);
    canvas.fill_rect(x, y, (w as f32 * level.min(1.0)) as u32, 4, if level > 0.95 { theme().red } else { theme().green });
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, setup: &Setup, output: &Output, message: &str) {
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, EDGE);
    let mut title = Line::<96>::new();
    let _ = write!(title, "FOSFOR PHONO  {}", CURVES[setup.curve].name);
    canvas.text_scaled(header.x + layout.margin, header.y + layout.margin, title.as_str(), layout.scale as u32, ACTIVE);
    let curve = &CURVES[setup.curve];
    let mut detail = Line::<96>::new();
    let groove = if curve.groove == Groove::Micro { "microgroove" } else { "78 rpm" };
    let _ = write!(detail, "{}  {}  {} Hz", curve.period, groove, output.rate);
    canvas.text(header.x + layout.margin, header.y + header.h - layout.margin - GLYPH_H, detail.as_str(), INK);
    let right = header.x + header.w - layout.margin;
    if !message.is_empty() {
        canvas.text_right(right, header.y + layout.margin, message, INK);
    }
    let meter_w = header.w * 14 / 100;
    let y = header.y + header.h - layout.margin - 4;
    meter(canvas, right - 2 * meter_w - layout.margin, y, meter_w, output.input_peak);
    meter(canvas, right - meter_w, y, meter_w, output.peak);
}

fn draw_plot(canvas: &mut Canvas, layout: &Layout, setup: &Setup, eq: &Phono) {
    let plot = layout.plot;
    canvas.fill_rect(plot.x, plot.y, plot.w as u32, plot.h as u32, 0x080b0f);
    let mut db = DB_MIN;
    while db <= DB_MAX {
        let y = layout.plot_y(db);
        canvas.line(plot.x, y, plot.x + plot.w - 1, y, if db == 0.0 { EDGE } else { GRID });
        if (db as i32) % 20 == 0 {
            let mut text = Line::<8>::new();
            let _ = write!(text, "{}", db as i32);
            canvas.text(plot.x + 2, y - GLYPH_H - 1, text.as_str(), REFERENCE);
        }
        db += 10.0;
    }
    for (hz, text) in [(20.0, "20"), (100.0, "100"), (1000.0, "1k"), (10000.0, "10k")] {
        let x = layout.plot_x(hz);
        canvas.line(x, plot.y, x, plot.y + plot.h - 1, GRID);
        canvas.text(x + 2, plot.y + plot.h - GLYPH_H - 1, text, REFERENCE);
    }
    // The analogue curve dim, the live (digital, with the filters) response over it.
    let (mut last_ref, mut last_live) = (None, None);
    for x in 0..plot.w {
        let position = x as f32 / (plot.w - 1) as f32;
        let hz = 20.0 * dsp::math::pow10(position * 3.0);
        let reference = (plot.x + x, layout.plot_y(curve_db(&CURVES[setup.curve], hz)));
        let live = (plot.x + x, layout.plot_y(eq.response_db(hz)));
        if let Some((px, py)) = last_ref {
            canvas.line(px, py, reference.0, reference.1, REFERENCE);
        }
        if let Some((px, py)) = last_live {
            canvas.line(px, py, live.0, live.1, LIVE);
        }
        last_ref = Some(reference);
        last_live = Some(live);
    }
    canvas.rect(plot.x, plot.y, plot.w as u32, plot.h as u32, EDGE);
}

fn draw_controls(canvas: &mut Canvas, layout: &Layout, setup: &Setup) {
    for widget in layout.widgets() {
        let rect = widget.rect;
        match widget.hit {
            Hit::Slider(index) => {
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, BUTTON_OFF);
                canvas.fill_rect(rect.x, rect.y, (rect.w as f32 * slider_value(setup, index)) as u32, rect.h as u32, BAR);
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                let mut text = Line::<48>::new();
                slider_text(setup, index, &mut text);
                center(canvas, rect, text.as_str(), layout.scale, INK);
            }
            hit => {
                let on = lit(setup, hit);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { ACTIVE } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                center(canvas, rect, label(hit), layout.scale, if on { 0x000000 } else { INK });
            }
        }
    }
}

struct PhonoTouch<'a> {
    layout: &'a Layout,
    setup: &'a mut Setup,
    save: &'a mut bool,
}

impl TouchTarget for PhonoTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        if hit == Hit::Save {
            *self.save = true;
        } else {
            press(self.layout, self.setup, hit, x);
        }
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, x: i32, _y: i32) -> Hit {
        drag(self.layout, self.setup, grab, x);
        grab
    }

    fn up(&mut self, _grab: Hit) {}
}

fn save(api: &TapeApi, config: &tape_rt::config::Config, setup: &Setup, message: &mut Line<64>) {
    let mut body = tape_rt::config::Settings::<320>::new();
    setup.write(&mut body);
    message.clear();
    match config.save(api, body.as_str()) {
        Ok(name) => {
            let _ = write!(message, "saved {name}");
        }
        Err(reason) => {
            let _ = write!(message, "save: {reason}");
        }
    }
}

/// Keys that mean the same in both modes; true when something changed.
fn handle_key(key: Key, setup: &mut Setup) -> bool {
    match key {
        Key::Char(b'm') => setup.mono = !setup.mono,
        Key::Char(b'l') => setup.set_inversion(!setup.invert_left(), setup.invert_right()),
        Key::Char(b'r') => setup.set_inversion(setup.invert_left(), !setup.invert_right()),
        Key::Char(b'[') => setup.curve = (setup.curve + CURVES.len() - 1) % CURVES.len(),
        Key::Char(b']') => setup.curve = (setup.curve + 1) % CURVES.len(),
        _ => return false,
    }
    true
}

/// `device=`, `outdev=` and the hard-float check; the screen is the caller's business.
fn startup(api: &'static TapeApi) -> Result<&'static str, &'static str> {
    tape_rt::init(api);
    let arguments = tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or(""))?;
    if let Some(number) = tape_rt::arg_value(arguments, "outdev") {
        let number = tape_rt::audio_device_number(api, number).map_err(|_| "outdev= needs a number (`audio` lists the devices) or loopN")?;
        tape_rt::use_audio_output(api, number)?;
    }
    if tape_rt::fp_level() == 0 {
        return Err("needs a hard-float tape build (kernel without FP support?)");
    }
    Ok(arguments)
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let arguments = match startup(api) {
        Ok(arguments) => arguments,
        Err(reason) => return println!("phono: {reason}."),
    };
    let config = match tape_rt::config::Config::load(api, TAPE_NAME, arguments) {
        Ok(config) => config,
        Err(reason) => return println!("phono: {reason}."),
    };
    let service = config.text().split_ascii_whitespace().any(|word| word == "service") || config.value("mode") == Some("service");
    let mut setup = Setup::default();
    if let Err(reason) = setup.read(config.text()) {
        return println!("phono: {}.", reason.as_str());
    }
    let mut output = match Output::start_duplex(api, config.text()) {
        Ok(output) => output,
        Err(reason) => return println!("phono: {}.", reason.as_str()),
    };
    let mut eq = Phono::new(output.rate);
    setup.apply(&mut eq);

    let screen_number = tape_rt::screen_arg(arguments);
    let mut graphics = None;
    if !service {
        let Some((width, height)) = tape_rt::gfx_info_on(screen_number) else {
            output.stop();
            return println!("phono: no pixel screen (BIOS text mode); `phono service` needs none.");
        };
        let layout = Layout::new(width.min(MAX_WIDTH) as i32, height.min(MAX_HEIGHT) as i32);
        let Some(screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(screen_number)) else {
            output.stop();
            return println!("phono: out of memory.");
        };
        if !screen.open() {
            output.stop();
            return println!("phono: cannot take the screen.");
        }
        graphics = Some((layout, screen));
    }
    println!(
        "phono: {} {}, {} Hz, {} ms queued, outputs {}-{}. Escape quits.",
        if service { "service," } else { "screen," },
        CURVES[setup.curve].id,
        output.rate,
        output.latency_ms,
        output.first + 1,
        output.first + 2
    );
    output.prime(|frames| frames.fill([0.0; 2]));

    let mut decoder = KeyDecoder::new();
    let mut touches = graphics.as_ref().map(|(layout, _)| Touches::new(layout.width, layout.height, screen_number));
    let mut message = Line::<64>::new();
    let mut next_animation = 0u64;
    let mut clipped = 0u32;
    loop {
        let mut changed = false;
        let mut save_requested = false;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            if key == Key::Char(b'w') {
                save_requested = true;
            } else {
                changed |= handle_key(key, &mut setup);
            }
        });
        if !keep_going {
            break;
        }
        if let (Some((layout, _)), Some(touches)) = (graphics.as_ref(), touches.as_mut()) {
            changed |= touches.poll(&mut PhonoTouch { layout, setup: &mut setup, save: &mut save_requested });
        }
        if save_requested && graphics.is_some() {
            save(api, &config, &setup, &mut message);
            changed = true;
        } else if changed {
            message.clear();
        }
        if changed {
            setup.apply(&mut eq);
        }
        output.pump_duplex(|frames| eq.process_frames(frames));
        if output.peak >= 1.0 {
            clipped += 1;
        }
        let Some((layout, screen)) = graphics.as_mut() else {
            tape_rt::idle();
            continue;
        };
        let now = tape_rt::micros();
        if (output.input_peak > 0.002 || output.peak > 0.002) && now >= next_animation {
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
                draw_header(&mut canvas, layout, &setup, &output, message.as_str());
                draw_plot(&mut canvas, layout, &setup, &eq);
                draw_controls(&mut canvas, layout, &setup);
            }
            blit_banded(screen, || {
                output.pump_duplex(|frames| eq.process_frames(frames));
            });
        }
        tape_rt::idle();
    }
    output.stop();
    if let Some((_, screen)) = graphics.as_mut() {
        screen.close();
    }
    if clipped > 0 {
        println!("phono: stopped; the output reached full scale {clipped} times (lower trim= or bass=).");
    } else {
        println!("phono: stopped.");
    }
}
