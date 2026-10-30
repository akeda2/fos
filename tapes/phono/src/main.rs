//! `phono`: a phono equaliser. A flat (unequalised) cartridge or preamp signal comes in on the audio device, goes through the
//! chosen record equalisation curve (microgroove or 78 rpm), an optional rumble filter, an adjustable bass boost, mono and
//! polarity switches, and goes out again. The sound is `crates/phono`; this file runs it, headless (`service`) or with the
//! curves and switches on the touch screen.
#![no_std]
#![no_main]

mod take;

use audio_screen::{Screen, poll_keys};
use core::fmt::Write;
use gfx::Canvas;
use gfx::theme::theme;
use instrument_rt::{Output, Rect, TouchTarget, Touches, blit_banded};
use outfx::{Mode, Surround};
use phono::{CURVES, Phono, curve_db, curves::Groove};
use phono_tape::{DB_MAX, DB_MIN, GLYPH_H, Hit, Layout, Setup, TAPE_NAME, drag, label, lit, press, slider_text, slider_value};
use take::Archive;
use tape_rt::{Key, KeyDecoder, Line, TapeApi, println};

tape_rt::tape_help!(
    br#"phono [service] [@FILE] [curve=ID] [mono=on|off] [polarity=P] [rumble=HZ]
[bass=DB] [bassfreq=HZ] [bassq=Q] [trim=DB]
[surround=off|sq|qs|ambience] [logic=off|music|film] [swap=on|off]
[layout=4.0|4.1|5.1] [lfe=HZ] [delay=MS] [name=NAME] [depth=auto|16|18|20|24]
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
surround= decodes the equalised stereo to 4 channels: sq or qs (matrix
records), ambience (stereo to quad, for classical and organ). logic= steers
the decoded channels (music: gentle, film: strong). swap=on mirrors the rear
pair when the rears come out reversed. layout= 4.0, 4.1 (LFE on out+2) or
5.1 (C silent, LFE on out+3); lfe= 40-200 Hz. The device needs the channels
from out= on; otherwise phono plays stereo and says so.
Archive: Record (key t) writes the captured Lt/Rt as NAME.lr.flac, flat, as
delivered; the raw master. Record again stops it. Decode (key d) turns the
latest take into NAME.flac with the equalisation and the chosen surround
decode (4, 4.1 or 6 channels), at the bit depth depth= (auto picks it from
the take's noise floor; b cycles it). It needs no extra outputs and listening
goes on. Name (key n) edits NAME from the keyboard (Enter keeps, Escape
cancels): lowercase letters, digits . _ -, at most 6 on FOSFS or up to 40
after disk2/. A taken name gets a number (lp2.lr.flac, lp2.flac).
Keys: m mono, l / r invert, [ ] previous / next curve, s surround, g logic,
x swap rear, o layout, t record, d decode, b depth, n name, w save; Escape
quits.
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
const RECORDING: u32 = 0xff6060;

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = gfx::text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn meter(canvas: &mut Canvas, x: i32, y: i32, w: i32, level: f32) {
    canvas.fill_rect(x, y, w as u32, 4, 0x080b0f);
    canvas.fill_rect(x, y, (w as f32 * level.min(1.0)) as u32, 4, if level > 0.95 { theme().red } else { theme().green });
}

fn draw_header(canvas: &mut Canvas, layout: &Layout, setup: &Setup, output: &Output, width: usize, status: &str, message: &str) {
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, PANEL);
    canvas.rect(header.x, header.y, header.w as u32, header.h as u32, EDGE);
    let mut title = Line::<96>::new();
    let _ = write!(title, "FOSFOR PHONO  {}", CURVES[setup.curve].name);
    canvas.text_scaled(header.x + layout.margin, header.y + layout.margin, title.as_str(), layout.scale as u32, ACTIVE);
    let curve = &CURVES[setup.curve];
    let mut detail = Line::<128>::new();
    let groove = if curve.groove == Groove::Micro { "microgroove" } else { "78 rpm" };
    let _ = write!(detail, "{}  {}  {} Hz", curve.period, groove, output.rate);
    if setup.surround.mode != Mode::Off {
        let _ = write!(detail, "  {}", setup.surround.mode.name());
        if width > 2 {
            let _ = write!(detail, " {}ch", width);
        } else {
            let _ = write!(detail, " (device has too few outputs)");
        }
    }
    if !status.is_empty() {
        let _ = write!(detail, "  {status}");
    }
    canvas.text(header.x + layout.margin, header.y + header.h - layout.margin - GLYPH_H, detail.as_str(), if status.is_empty() { INK } else { RECORDING });
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

fn draw_controls(canvas: &mut Canvas, layout: &Layout, setup: &Setup, archive: &Archive) {
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
            Hit::Name => {
                let editing = archive.editing.as_ref();
                let shown = editing.unwrap_or(&archive.name);
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if editing.is_some() { ACTIVE } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                let mut text = Line::<64>::new();
                let _ = write!(text, "Name {}{}", shown.as_str(), if editing.is_some() { "_" } else { "" });
                // A long name loses its beginning, so the end being typed stays visible.
                let fits = ((rect.w - 4) / (layout.scale * 8)).max(1) as usize;
                let from = text.length.saturating_sub(fits);
                center(canvas, rect, &text.as_str()[from..], layout.scale, if editing.is_some() { 0x000000 } else { INK });
            }
            hit => {
                let (mut on, mut fill) = (lit(setup, hit), ACTIVE);
                let mut text = Line::<24>::new();
                let _ = text.write_str(label(setup, hit));
                if hit == Hit::Record && archive.recording {
                    (on, fill) = (true, RECORDING);
                    text.clear();
                    let _ = write!(text, "Stop rec");
                } else if hit == Hit::Decode && archive.decoding() {
                    on = true;
                    text.clear();
                    let _ = write!(text, "Decoding {}%", archive.percent);
                }
                canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { fill } else { BUTTON_OFF });
                canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, EDGE);
                center(canvas, rect, text.as_str(), layout.scale, if on { 0x000000 } else { INK });
            }
        }
    }
}

struct PhonoTouch<'a> {
    layout: &'a Layout,
    setup: &'a mut Setup,
    save: &'a mut bool,
    /// Record, Decode or Name: the main loop owns what they do.
    action: &'a mut Option<Hit>,
}

impl TouchTarget for PhonoTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        if hit == Hit::Save {
            *self.save = true;
        } else if matches!(hit, Hit::Record | Hit::Decode | Hit::Name) {
            *self.action = Some(hit);
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

fn save(api: &TapeApi, config: &tape_rt::config::Config, setup: &Setup, name: &str, message: &mut Line<64>) {
    let mut body = tape_rt::config::Settings::<512>::new();
    setup.write(&mut body);
    body.put("name", format_args!("{name}"));
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
        Key::Char(b's') => setup.surround.mode = setup.surround.mode.next(),
        Key::Char(b'g') => press_logic(setup),
        Key::Char(b'x') => setup.surround.swap_rear = !setup.surround.swap_rear,
        Key::Char(b'o') => setup.cycle_speakers(),
        _ => return false,
    }
    true
}

fn say_unchanged(message: &mut Line<64>) {
    message.clear();
    let _ = write!(message, "name unchanged");
}

fn press_logic(setup: &mut Setup) {
    let layout = Layout::new(640, 480);
    press(&layout, setup, Hit::Logic, 0);
}

/// Channels to play for the chosen surround settings: stereo when the device lacks the outputs.
fn play_width(setup: &Setup, output: &Output) -> usize {
    let wanted = setup.surround.channels();
    if wanted > 2 && output.first + wanted <= output.channels { wanted } else { 2 }
}

/// Equalises the captured frames; with a wider output also decodes them into the surround channels.
fn pump(output: &mut Output, eq: &mut Phono, surround: &mut Surround, width: usize, archive: &mut Archive) {
    if width > 2 {
        output.pump_duplex_wide(width, |frames, wide| {
            archive.tap(frames);
            eq.process_frames(frames);
            for (frame, out) in frames.iter().zip(wide.iter_mut()) {
                *out = surround.process(*frame);
            }
        });
    } else {
        output.pump_duplex(|frames| {
            archive.tap(frames);
            eq.process_frames(frames);
        });
    }
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
    // About 64 KiB (the ambience delay line): a static, not stack.
    static mut SURROUND: Surround = Surround::new();
    let surround = unsafe { &mut *core::ptr::addr_of_mut!(SURROUND) };
    surround.tune(output.rate, setup.surround);
    let mut width = play_width(&setup, &output);
    let name = config.value("name").unwrap_or(take::DEFAULT_NAME);
    if let Err(error) = archive::Names::parse(name) {
        output.stop();
        return println!("phono: name={name} is not usable ({error:?}); lowercase letters, digits . _ - and diskN/ or @LABEL/.");
    }
    let mut archive = Archive::new(api, name);

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
        output.first + width.max(2)
    );
    output.prime(|frames| frames.fill([0.0; 2]));
    let mut status = Line::<48>::new();

    let mut decoder = KeyDecoder::new();
    let mut touches = graphics.as_ref().map(|(layout, _)| Touches::new(layout.width, layout.height, screen_number));
    let mut message = Line::<64>::new();
    let mut cursor = None::<(i32, i32)>;
    let mut next_animation = 0u64;
    let mut clipped = 0u32;
    loop {
        let mut changed = false;
        let mut save_requested = false;
        let mut action = None;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            if archive.editing.is_some() {
                archive.edit_key(key, &mut message);
                changed = true;
                return;
            }
            match key {
                Key::Char(b'w') => save_requested = true,
                Key::Char(b't') => action = Some(Hit::Record),
                Key::Char(b'd') => action = Some(Hit::Decode),
                Key::Char(b'n') => action = Some(Hit::Name),
                Key::Char(b'b') => {
                    press(&Layout::new(640, 480), &mut setup, Hit::Depth, 0);
                    changed = true;
                }
                _ => changed |= handle_key(key, &mut setup),
            }
        });
        if !keep_going {
            // Escape while the name is edited only drops the edit.
            if archive.editing.take().is_none() {
                break;
            }
            say_unchanged(&mut message);
            changed = true;
        }
        if let (Some((layout, _)), Some(touches)) = (graphics.as_ref(), touches.as_mut()) {
            let mut target = PhonoTouch { layout, setup: &mut setup, save: &mut save_requested, action: &mut action };
            while let Some(event) = tape_rt::touch_event() {
                let Some(extent) = tape_rt::touch_extent(event.panel) else { continue };
                if event.is_mouse() && (event.screen < 0 || event.screen == screen_number as i32) {
                    cursor = Some((event.x * layout.width / extent.0.max(1), event.y * layout.height / extent.1.max(1)));
                    changed = true;
                }
                changed |= touches.handle(&event, extent, &mut target);
            }
        }
        if save_requested && graphics.is_some() {
            save(api, &config, &setup, archive.name.as_str(), &mut message);
            changed = true;
        } else if changed && action.is_none() {
            message.clear();
        }
        if let Some(hit) = action {
            changed = true;
            match hit {
                Hit::Record if archive.recording => archive.stop_recording(&mut output, &mut message),
                Hit::Record => archive.start_recording(&setup, &mut output, &mut message),
                Hit::Decode if archive.decoding() => archive.cancel_decode(&mut message),
                Hit::Decode => archive.start_decode(&setup, &mut message),
                _ => archive.begin_edit(),
            }
        }
        if changed {
            setup.apply(&mut eq);
            surround.tune(output.rate, setup.surround);
            width = play_width(&setup, &output);
            if setup.surround.mode != Mode::Off && width == 2 && message.is_empty() {
                let _ = write!(message, "device has {} outputs, need {}", output.channels - output.first, setup.surround.channels());
            }
        }
        pump(&mut output, &mut eq, surround, width, &mut archive);
        if output.peak >= 1.0 {
            clipped += 1;
        }
        changed |= archive.service(&mut output, &mut message);
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
                archive.status(&mut status);
                draw_header(&mut canvas, layout, &setup, &output, width, status.as_str(), message.as_str());
                draw_plot(&mut canvas, layout, &setup, &eq);
                draw_controls(&mut canvas, layout, &setup, &archive);
                if let Some((x, y)) = cursor {
                    canvas.cursor(x, y, layout.scale as u32 + 1, INK, BACKGROUND);
                }
            }
            blit_banded(screen, || pump(&mut output, &mut eq, surround, width, &mut archive));
        }
        tape_rt::idle();
    }
    let mut closing = Line::<64>::new();
    archive.shut_down(&mut output, &mut closing);
    if !closing.is_empty() {
        println!("phono: {}.", closing.as_str());
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
