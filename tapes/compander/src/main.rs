//! `compander`: a stereo dbx Type I / Type II compander as a stand-alone box. Audio in -> encode (the record side, in front of a
//! tape machine) or decode (the playback side, behind it) -> audio out, on one duplex audio device, with level meters and a
//! gain meter on the pixel screen and the mode and type on touch buttons. The sound is `crates/compander`; this file runs it.
#![no_std]
#![no_main]

use audio_screen::{Screen, poll_keys};
use compander::Mode;
use core::fmt::Write;
use compander::tapeeq::{Speed, Standard};
use dsp::math::log10;
use compander_tape::{BUTTONS, EqState, GLYPH_H, Hit, Kind, Layout, Source, Style, TONE_MAX_DB, TONE_MIN_DB, Tone, Unit, eq_state, eq_text, label, lit, mode_from_name, mode_name, press};
use gfx::meter::{Scale, draw_bar, draw_bipolar, draw_scale};
use gfx::theme::theme;
use gfx::{Canvas, text_width};
use ltas::{Analyzer, Config, Mode as View, Resolution};
use instrument_rt::{Output, Rect, TouchTarget, Touches, blit_banded};
use tape_rt::{Key, KeyDecoder, Line, TapeApi, alloc_zeroed, println};

tape_rt::tape_help!(
    br#"compander [type=1|2] [mode=bypass|encode|decode] [speed=1.875|3.75|7.5|15|30]
    [machine=nab|iec|120|70] [tape=nab|iec|120|70] [trim=DB]
    [tone=off|100|1k|10k|pink] [level=DB] [view=spectrum] [style=bars|plot]
    [source=in|out] [device=N|loopN] [outdev=N|loopN] [screen=N] [rate=HZ]
    [latency=MS] [in=N] [out=N] [theme=dark|classic|light]
A stereo dbx noise-reduction compander as a stand-alone box: whatever comes in
on the audio device is encoded (2:1 compression, for recording to tape) or
decoded (1:2 expansion, for playback) and goes out again. Both types are one
true-RMS, log-domain detector (a quadrature network, 60 dB/s recovery); Type II
adds the band-limited, treble-weighted detector for cassettes. Types are not
compatible: decode with the type you encoded with. Alignment tones should not
be encoded. The bars show input and output peaks, the gain bar the dB the
compander applies now (compression left, expansion right).
Tape equalisation: speed= is the tape speed (1.875 is the compact cassette),
machine= the standard the machine's equalisers are aligned to and tape= the
one the tape was recorded to: NAB or IEC (CCIR/DIN) for reels, 120 (ferric,
Type I) or 70 (chrome and metal) microseconds for cassettes. When they differ,
Decode corrects playback and Encode pre-corrects recording; Bypass leaves the
signal alone. The EQ bar says which is happening and by how much.
trim=DB is an output trim (+-12, all modes). tone= sends an alignment tone
(level= dBFS, 0 = a full-scale sine, default -18; pink is pink noise) straight
out, past the compander, the equalisation and the trim.
The spectrum view analyses the input (a tape's return) or the output. Bars are
third octaves: play pink noise through a machine and a flat machine reads flat.
The plot is every FFT bin, unsmoothed, for overtones and hum. Hold freezes it.
latency=MS is the audio kept queued (10-200, default 40); in=N / out=N choose
the left channel of the pair (1-based).
Keys: b Bypass, e Encode, d Decode, 1 Type I, 2 Type II, t other type, s speed,
m machine, r tape standard, o tone, Left/Right tone level, Up/Down trim,
f spectrum, p bars/plot, i source, h hold; Escape quits. The corner
frequencies are from published sources and provisional (docs/compander.md).
"#
);

const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const GAIN_RANGE: f32 = 40.0;
const MAXN: usize = 8192;
const MAXBINS: usize = MAXN / 2 + 1;
static mut LEVELS: [[f32; MAXBINS]; 2] = [[0.0; MAXBINS]; 2];

type Analyzers = [&'static mut Analyzer<MAXN>; 2];

/// Feeds the analysers and runs the unit; the analysers see the input or the output, whichever the view shows.
fn run(unit: &mut Unit, analyzers: &mut Analyzers, frames: &mut [[f32; 2]]) {
    let watching = unit.spectrum() && !unit.hold();
    let before = watching && unit.source() == Source::Input;
    let after = watching && unit.source() == Source::Output;
    if before {
        feed(analyzers, frames);
    }
    unit.process_block(frames);
    if after {
        feed(analyzers, frames);
    }
}

fn feed(analyzers: &mut Analyzers, frames: &[[f32; 2]]) {
    let mut scratch = [[0.0f32; 256]; 2];
    for chunk in frames.chunks(256) {
        for (i, frame) in chunk.iter().enumerate() {
            scratch[0][i] = frame[0];
            scratch[1][i] = frame[1];
        }
        for c in 0..2 {
            analyzers[c].push(&scratch[c][..chunk.len()]);
        }
    }
}

fn configure(analyzers: &mut Analyzers, rate: u32, style: Style) -> bool {
    let mut config = Config::new(rate);
    config.n = MAXN;
    config.hop = MAXN / 2;
    config.tau_s = 1.0;
    config.resolution = if style == Style::Fine { Resolution::Fft } else { Resolution::Third };
    config.avx2 = tape_rt::fp_level() >= tape_rt::FP_AVX2;
    analyzers.iter_mut().all(|analyzer| analyzer.configure(config))
}

/// The spectrum: third-octave bars (left channel green, right amber, bands the FFT cannot resolve dim), or, unsmoothed, a line
/// through every FFT bin on a log frequency axis, for overtones, hum and noise lines that a band would hide.
fn draw_spectrum(canvas: &mut Canvas, layout: &Layout, analyzers: &mut Analyzers, unit: &Unit) {
    let top_y = layout.eq.y + layout.eq.h + layout.margin;
    let area = Rect { x: layout.margin, y: top_y, w: layout.width - 2 * layout.margin, h: layout.buttons[0].y - top_y - layout.margin };
    canvas.fill_rect(area.x, area.y, area.w as u32, area.h as u32, theme().grid);
    canvas.rect(area.x, area.y, area.w as u32, area.h as u32, theme().grid_major);
    let fine = unit.style() == Style::Fine;
    let range = if fine { 110.0 } else { 80.0 };
    let (left, top) = (area.x + 6 * layout.scale + 24, area.y + layout.margin);
    let plot_w = area.w - (left - area.x) - layout.margin;
    let plot_h = area.h - 2 * layout.margin - GLYPH_H - 6;
    let y_of = |db: f32| top + ((0.0 - db).clamp(0.0, range) / range * plot_h as f32) as i32;
    let mut db = 0.0;
    while db >= -range {
        let y = y_of(db);
        canvas.fill_rect(left, y, plot_w as u32, 1, if db == 0.0 { theme().grid_major } else { theme().off });
        let mut text = Line::<8>::new();
        let _ = write!(text, "{:.0}", db);
        canvas.text_right(left - 4, y - GLYPH_H / 2, text.as_str(), theme().dim);
        db -= 10.0;
    }
    let count = analyzers[0].outputs().min(MAXBINS);
    if count == 0 {
        return;
    }
    let levels = unsafe { &mut *core::ptr::addr_of_mut!(LEVELS) };
    for c in 0..2 {
        analyzers[c].levels_db(View::Exp, &mut levels[c]);
    }
    let held = if unit.hold() { "  HELD" } else { "" };
    let mut text = Line::<120>::new();
    if fine {
        let span = log10(20_000.0 / 20.0);
        let x_of = |hz: f32| left + (log10(hz / 20.0) / span * plot_w as f32) as i32;
        for (hz, name) in [(20.0, "20"), (50.0, "50"), (100.0, "100"), (200.0, "200"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k"), (5000.0, "5k"), (10_000.0, "10k"), (20_000.0, "20k")] {
            let x = x_of(hz).min(left + plot_w - 1);
            canvas.fill_rect(x, top, 1, plot_h as u32, theme().off);
            canvas.text_centered(x, top + plot_h + 6, name, theme().dim);
        }
        for c in 0..2 {
            let colour = if c == 0 { theme().green } else { theme().amber };
            let mut previous: Option<(i32, i32)> = None;
            for i in 0..count {
                let hz = analyzers[c].centre_hz(i);
                if hz < 20.0 || hz > 20_000.0 {
                    continue;
                }
                let point = (x_of(hz), y_of(levels[c][i]));
                if let Some((px, py)) = previous {
                    canvas.line(px, py, point.0, point.1, colour);
                }
                previous = Some(point);
            }
        }
        let _ = write!(text, "spectrum of {}, every FFT bin ({:.1} Hz), unsmoothed  L green  R amber{}", unit.source().name(), output_bin_hz(analyzers), held);
    } else {
        let slot = plot_w / count as i32;
        let bar = ((slot - 2) / 2).max(1);
        for i in 0..count {
            let x = left + i as i32 * slot + 1;
            let resolved = analyzers[0].band_is_resolved(i);
            for c in 0..2 {
                let y = y_of(levels[c][i]);
                let colour = if !resolved { theme().dim } else if c == 0 { theme().green } else { theme().amber };
                canvas.fill_rect(x + c as i32 * bar, y, bar as u32, (top + plot_h - y).max(0) as u32, colour);
            }
            let centre = analyzers[0].centre_hz(i);
            for (mark, name) in [(31.5, "31"), (63.0, "63"), (125.0, "125"), (250.0, "250"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k"), (4000.0, "4k"), (8000.0, "8k"), (16000.0, "16k")] {
                if centre > mark * 0.97 && centre < mark * 1.03 {
                    canvas.text_centered(x + bar, top + plot_h + 6, name, theme().dim);
                }
            }
        }
        let _ = write!(text, "spectrum of {} (1/3 octave, dB re full-scale sine)  L green  R amber{}", unit.source().name(), held);
    }
    canvas.text(left + 4, top + 2, text.as_str(), theme().text);
}

fn output_bin_hz(analyzers: &Analyzers) -> f32 {
    analyzers[0].config().sample_rate as f32 / analyzers[0].config().n as f32
}

fn center(canvas: &mut Canvas, rect: Rect, text: &str, scale: i32, rgb: u32) {
    let width = text_width(text) as i32 * scale;
    canvas.text_scaled(rect.x + (rect.w - width) / 2, rect.y + (rect.h - GLYPH_H * scale) / 2, text, scale as u32, rgb);
}

fn draw_strip(canvas: &mut Canvas, layout: &Layout, unit: &Unit, index: usize) {
    let strip = layout.strips[index];
    let scale = layout.scale;
    canvas.fill_rect(strip.x, strip.y, strip.w as u32, strip.h as u32, theme().grid);
    canvas.rect(strip.x, strip.y, strip.w as u32, strip.h as u32, theme().grid_major);
    let title = if index == 0 { "LEFT" } else { "RIGHT" };
    canvas.text_scaled(strip.x + layout.margin, strip.y + layout.margin, title, scale as u32, theme().text);

    let bar_w = 36 * scale;
    let gap = 28 * scale;
    let region_w = 2 * bar_w + gap;
    let region_x = strip.x + (strip.w - region_w) / 2;
    let bars_y = strip.y + GLYPH_H * scale + layout.margin + 34;
    let gain_block = 34 * scale + 40;
    let bars_h = (strip.h - (bars_y - strip.y) - gain_block - 20).max(40);
    draw_scale(canvas, &Scale::DBFS, region_x, region_w as u32, bars_y, bars_h as u32);
    let clipped = unit.clipped[index] > 0;
    draw_bar(canvas, &Scale::DBFS, region_x, bar_w as u32, bars_y, bars_h as u32, unit.input_db(index), -120.0, false);
    draw_bar(canvas, &Scale::DBFS, region_x + bar_w + gap, bar_w as u32, bars_y, bars_h as u32, unit.output_db(index), -120.0, clipped);
    canvas.text_centered(region_x + bar_w / 2, bars_y + bars_h + 4, "IN", theme().dim);
    canvas.text_centered(region_x + bar_w + gap + bar_w / 2, bars_y + bars_h + 4, "OUT", theme().dim);

    let gain = unit.gain_db(index);
    let gain_y = bars_y + bars_h + 28;
    draw_bipolar(canvas, strip.x + 3 * layout.margin, gain_y, (strip.w - 6 * layout.margin) as u32, (12 * scale) as u32, gain / GAIN_RANGE, false, "-40 dB", "+40 dB");
    let mut text = Line::<48>::new();
    let _ = write!(text, "gain {:+.1} dB   level {:.0} dBFS", gain, unit.level_db(index));
    canvas.text_centered(strip.x + strip.w / 2, gain_y + 12 * scale + 28, text.as_str(), theme().text);
}

fn draw(screen: &mut Screen, layout: &Layout, unit: &Unit, analyzers: &mut Analyzers, cursor: Option<(i32, i32)>) {
    let mut canvas = screen.frame();
    canvas.clear(theme().background);
    let header = layout.header;
    canvas.fill_rect(header.x, header.y, header.w as u32, header.h as u32, theme().grid);
    canvas.text_scaled(header.x + layout.margin, header.y + (header.h - GLYPH_H * layout.scale) / 2, "FOSFOR compander", layout.scale as u32, theme().text);
    let mut status = Line::<48>::new();
    let _ = write!(status, "{} {}", unit.kind().name(), mode_name(unit.mode()));
    let tint = if unit.mode() == Mode::Bypass { theme().dim } else { theme().green };
    let width = text_width(status.as_str()) as i32 * layout.scale;
    canvas.text_scaled(header.w - layout.margin - width, header.y + (header.h - GLYPH_H * layout.scale) / 2, status.as_str(), layout.scale as u32, tint);
    let eq = layout.eq;
    let (eq_colour, eq_fill) = match eq_state(unit) {
        EqState::Active => (theme().green, theme().grid_major),
        EqState::Bypassed => (theme().amber, theme().grid),
        EqState::Flat => (theme().dim, theme().grid),
    };
    canvas.fill_rect(eq.x, eq.y, eq.w as u32, eq.h as u32, eq_fill);
    canvas.rect(eq.x, eq.y, eq.w as u32, eq.h as u32, eq_colour);
    let eq_line = eq_text(unit);
    let eq_scale = if text_width(eq_line.as_str()) as i32 * layout.scale + 2 * layout.margin <= eq.w { layout.scale } else { 1 };
    canvas.text_scaled(eq.x + layout.margin, eq.y + (eq.h - GLYPH_H * eq_scale) / 2, eq_line.as_str(), eq_scale as u32, eq_colour);
    let mut info = Line::<96>::new();
    let _ = write!(info, "trim {:+.1} dB", unit.trim_db());
    if unit.tone() != Tone::Off {
        if unit.tone() == Tone::Pink {
            let _ = write!(info, "  TONE pink {:.0} dBFS", unit.tone_db());
        } else {
            let _ = write!(info, "  TONE {} Hz {:.0} dBFS", unit.tone().hz() as i32, unit.tone_db());
        }
    }
    canvas.text(header.x + header.w * 22 / 100, header.y + (header.h - GLYPH_H) / 2, info.as_str(), theme().dim);
    if unit.spectrum() {
        draw_spectrum(&mut canvas, layout, analyzers, unit);
    } else {
        draw_strip(&mut canvas, layout, unit, 0);
        draw_strip(&mut canvas, layout, unit, 1);
    }
    for (rect, hit) in layout.buttons.iter().zip(BUTTONS) {
        let on = lit(unit, hit);
        canvas.fill_rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { theme().grid_major } else { theme().grid });
        canvas.rect(rect.x, rect.y, rect.w as u32, rect.h as u32, if on { theme().green } else { theme().dim });
        center(&mut canvas, *rect, label(unit, hit).as_str(), layout.scale, theme().text);
    }
    if let Some((x, y)) = cursor {
        canvas.cursor(x, y, layout.scale as u32 + 1, theme().text, theme().background);
    }
}

struct DbxTouch<'a> {
    layout: &'a Layout,
    unit: &'a mut Unit,
    quit: &'a mut bool,
}

impl TouchTarget for DbxTouch<'_> {
    type Grab = Hit;

    fn down(&mut self, x: i32, y: i32) -> Option<Hit> {
        let hit = self.layout.hit(x, y)?;
        if !press(self.unit, hit) {
            *self.quit = true;
        }
        Some(hit)
    }

    fn moved(&mut self, grab: Hit, _x: i32, _y: i32) -> Hit {
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
            return println!("compander: {reason}.");
        }
    };
    if let Some(name) = tape_rt::arg_value(start.arguments, "theme") {
        if !gfx::theme::set_by_name(name) {
            return println!("compander: theme= takes dark, classic or light.");
        }
    }
    let kind = match tape_rt::arg_value(start.arguments, "type") {
        None => Kind::II,
        Some(name) => match Kind::from_name(name) {
            Some(kind) => kind,
            None => return println!("compander: type= takes 1 or 2."),
        },
    };
    let mode = match tape_rt::arg_value(start.arguments, "mode") {
        None => Mode::Bypass,
        Some(name) => match mode_from_name(name) {
            Some(mode) => mode,
            None => return println!("compander: mode= takes bypass, encode or decode."),
        },
    };
    let speed = match tape_rt::arg_value(start.arguments, "speed") {
        None => Speed::Ips15,
        Some(name) => match Speed::from_name(name) {
            Some(speed) => speed,
            None => return println!("compander: speed= takes 1.875 (cassette), 3.75, 7.5, 15 or 30."),
        },
    };
    let standard = |key: &str| match tape_rt::arg_value(start.arguments, key) {
        None => Ok(Standard::Nab),
        Some(name) => Standard::from_name(name).ok_or(()),
    };
    let (Ok(machine), Ok(tape)) = (standard("machine"), standard("tape")) else {
        return println!("compander: machine= and tape= take nab, iec (ccir), 120 or 70.");
    };
    let tone = match tape_rt::arg_value(start.arguments, "tone") {
        None => Tone::Off,
        Some(name) => match Tone::from_name(name) {
            Some(tone) => tone,
            None => return println!("compander: tone= takes off, 100, 1k, 10k or pink."),
        },
    };
    let source = match tape_rt::arg_value(start.arguments, "source") {
        None | Some("in") => Source::Input,
        Some("out") => Source::Output,
        Some(_) => return println!("compander: source= takes in or out."),
    };
    let view = match tape_rt::arg_value(start.arguments, "view") {
        None | Some("meters") => false,
        Some("spectrum") => true,
        Some(_) => return println!("compander: view= takes meters or spectrum."),
    };
    let style = match tape_rt::arg_value(start.arguments, "style") {
        None | Some("bars") => Style::Bands,
        Some("plot") => Style::Fine,
        Some(_) => return println!("compander: style= takes bars or plot."),
    };
    let trim = tape_rt::arg_value(start.arguments, "trim").and_then(tape_rt::parse_f32);
    let tone_db = tape_rt::arg_value(start.arguments, "level").and_then(tape_rt::parse_f32);
    let mut output = match Output::start_duplex(api, start.arguments) {
        Ok(output) => output,
        Err(reason) => return println!("compander: {}.", reason.as_str()),
    };
    let mut unit = Unit::new(output.rate, kind, mode);
    unit.set_speed(speed);
    unit.set_machine(machine);
    unit.set_tape(tape);
    unit.set_tone(tone);
    unit.set_trim_db(trim.unwrap_or(0.0));
    unit.set_tone_db(tone_db.unwrap_or(-18.0).clamp(TONE_MIN_DB, TONE_MAX_DB));
    unit.set_source(source);
    unit.set_spectrum(view);
    let (Some(first), Some(second)) = (alloc_zeroed::<Analyzer<MAXN>>(api), alloc_zeroed::<Analyzer<MAXN>>(api)) else {
        output.stop();
        return println!("compander: out of memory.");
    };
    let mut analyzers: Analyzers = [first, second];
    unit.set_style(style);
    if !configure(&mut analyzers, output.rate, style) {
        output.stop();
        return println!("compander: cannot configure the analyser at {} Hz.", output.rate);
    }
    let mut watched = (view, source, style);
    let layout = Layout::new(start.width as i32, start.height as i32);
    let Some(mut screen) = Screen::new(api, layout.width as u32, layout.height as u32).map(|screen| screen.on(start.screen)) else {
        output.stop();
        return println!("compander: out of memory.");
    };
    if !screen.open() {
        output.stop();
        return println!("compander: cannot take the screen.");
    }
    println!("compander: {} Hz, {} {}, {} ips, tape {} on machine {}, in -> compander -> out outputs {}-{}. Escape quits.", output.rate, kind.name(), mode_name(mode), speed.name(), tape.label(speed), machine.label(speed), output.first + 1, output.first + 2);
    output.prime(|frames| frames.fill([0.0; 2]));

    let mut decoder = KeyDecoder::new();
    let mut touches = Touches::new(layout.width, layout.height, start.screen);
    let mut quit = false;
    let mut next_animation = 0u64;
    while !quit {
        let mut changed = false;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            let hit = match key {
                Key::Char(b'b') => Hit::Mode(Mode::Bypass),
                Key::Char(b'e') => Hit::Mode(Mode::Encode),
                Key::Char(b'd') => Hit::Mode(Mode::Decode),
                Key::Char(b'1') => Hit::Kind(Kind::I),
                Key::Char(b'2') => Hit::Kind(Kind::II),
                Key::Char(b't') => Hit::Kind(if unit.kind() == Kind::I { Kind::II } else { Kind::I }),
                Key::Char(b'o') => Hit::Tone,
                Key::Char(b'f') => Hit::Spectrum,
                Key::Char(b'i') => Hit::Source,
                Key::Char(b'p') => Hit::Style,
                Key::Char(b'h') => Hit::Hold,
                Key::Char(b's') => Hit::Speed,
                Key::Char(b'm') => Hit::Machine,
                Key::Char(b'r') => Hit::Tape,
                Key::Up => Hit::Trim(1),
                Key::Down => Hit::Trim(-1),
                Key::Right => Hit::ToneLevel(1),
                Key::Left => Hit::ToneLevel(-1),
                _ => return,
            };
            press(&mut unit, hit);
            changed = true;
        });
        if !keep_going {
            break;
        }
        changed |= touches.poll(&mut DbxTouch { layout: &layout, unit: &mut unit, quit: &mut quit });

        let now_watching = (unit.spectrum(), unit.source(), unit.style());
        if now_watching != watched {
            if now_watching.2 != watched.2 {
                configure(&mut analyzers, output.rate, now_watching.2);
            }
            watched = now_watching;
            analyzers[0].reset();
            analyzers[1].reset();
        }
        output.pump_duplex(|frames| run(&mut unit, &mut analyzers, frames));
        let now = tape_rt::micros();
        if now >= next_animation {
            next_animation = now + 50_000;
            changed = true;
        }
        if changed {
            screen.mark_dirty();
        }
        if screen.due() {
            draw(&mut screen, &layout, &unit, &mut analyzers, touches.pointer());
            blit_banded(&screen, || {
                output.pump_duplex(|frames| run(&mut unit, &mut analyzers, frames));
            });
        }
        tape_rt::idle();
    }
    output.stop();
    screen.close();
    println!("compander: stopped; {} and {} sample(s) limited.", unit.clipped[0], unit.clipped[1]);
}
