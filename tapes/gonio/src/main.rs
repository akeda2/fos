//! `gonio`: a broadcast-style goniometer for a pair of audio input channels: a phosphor-persistence phase scope in the middle,
//! vertical Nordic PPM / dBFS level meters on the right, a correlation meter and a balance bar under the scope, and a loudness
//! (LUFS) panel left of the scope. The pieces are libraries: the scaffold `crates/audio-screen`, the scope, meters
//! and plot helpers in `crates/gfx`, the measurements in `crates/dsp` (`stereo`, `level`) and the Nordic PPM in
//! `audio_wire::meter`. A hard-float tape.
#![no_std]
#![no_main]

mod layout;
mod lufs;

use audio_screen::meters::{Channel, Kind as MeterKind};
use audio_screen::{Input, Screen, poll_keys};
use core::fmt::Write;
use dsp::level::db_amplitude;
use dsp::loudness::Loudness;
use dsp::stereo::Correlator;
use gfx::meter;
use gfx::theme::theme;
use gfx::scope::{LOG_RANGE_DB, Law, Phosphor, draw_graticule};
use layout::Layout;
use lufs::Panel;
use noise::PairSine;
use tape_rt::{Key, KeyDecoder, Line, TapeApi, alloc_bytes, arg_value as value, parse_f32, println};

tape_rt::tape_help!(
    br#"gonio [@NAME] [device=N] [chan=A,B] [rate=48000] [scale=nordic|dbfs]
      [style=dots|lines] [law=log|linear] [range=DB] [gain=DB] [tau=SECONDS]
      [target=-23|-24|-14] [ebu=9|18] [theme=dark|classic|light]
      [src=input|test] [phase=DEG] [tone=HZ] [level=-18] [right=DB]
Goniometer for two input channels (default 1,2; `device=` picks the audio
device, `audio` numbers them): phase scope with persistence (mono is a
vertical line, L only leans left, opposite polarity is horizontal), peak
meters for each channel (Nordic PPM, TEST = -18 dBFS, or sample peak in
dBFS, with peak hold and a clip lamp), the correlation (-1..+1) over about
tau seconds (default 0.3) and the L/R balance. src=test shows a built-in
stereo tone (phase=DEG between L and R, right=DB level difference) instead.
The left side is a loudness (EBU R128, ITU BS.1770 LUFS) panel: M (400 ms),
S (3 s), I (integrated, gated), LRA and a one-per-second history of S.
Its bar reads in LU against the target (-23 EBU, -24 ATSC, -14 streaming) on
the EBU +9 or +18 scale, one bar for M and one for S.
Keys (Escape quits):
  space freeze   r reset (holds, clip lamps, scope)   m Nordic/dBFS scale
  v dots/lines   l log/linear scope law   x log range 30/40/50/60 dB
               (log: the centre is -range dBFS, quiet signals stay visible)
  + - scope gain 3 dB   , . input pair -/+ (1-2, 3-4, ...)
  t loudness target -23/-24/-14   e EBU scale +9/+18
  i reset I and LRA   k colour theme dark/classic/light
  w save the settings (scale, style, law, range, gain, tau, rate, target, ebu) in gonio.cfg
  p test tone phase 0/45/90/135/180   (src=test)
Settings: gonio.cfg is read at start (key=value words, # comments); `@NAME` on
the command line reads that file instead (any diskN/NAME) and w saves to it.
Words on the command line win over the file. An unset key keeps its default.
"#
);

const BLOCK: usize = 32 << 10;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const PHASES: [f32; 5] = [0.0, 45.0, 90.0, 135.0, 180.0];

static mut BLOCK_BYTES: [u8; BLOCK] = [0; BLOCK];
static mut LEFT: [f32; BLOCK / 2] = [0.0; BLOCK / 2];
static mut RIGHT: [f32; BLOCK / 2] = [0.0; BLOCK / 2];

struct State {
    rate: u32,
    kind: MeterKind,
    lines: bool,
    gain_db: f32,
    tau_s: f32,
    frozen: bool,
    test: bool,
    phase_index: usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    let started = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(started) => started,
        Err(reason) => return println!("gonio: {reason}."),
    };
    let config = match tape_rt::config::Config::load(api, "gonio", started.arguments) {
        Ok(config) => config,
        Err(reason) => return println!("gonio: {reason}."),
    };
    let (arguments, width, height) = (config.text(), started.width, started.height);
    if let Some(name) = value(arguments, "theme") {
        if !gfx::theme::set_by_name(name) {
            return println!("gonio: theme= takes dark, classic or light.");
        }
    }
    let mut state = State {
        rate: value(arguments, "rate").and_then(|v| v.parse().ok()).unwrap_or(48_000),
        kind: if value(arguments, "scale") == Some("dbfs") { MeterKind::Dbfs } else { MeterKind::Nordic },
        lines: value(arguments, "style") == Some("lines"),
        gain_db: value(arguments, "gain").and_then(parse_f32).unwrap_or(0.0).clamp(-12.0, 36.0),
        tau_s: value(arguments, "tau").and_then(parse_f32).unwrap_or(0.3).clamp(0.02, 5.0),
        frozen: false,
        test: value(arguments, "src") == Some("test"),
        phase_index: 0,
    };
    if let Some(src) = value(arguments, "src") {
        if src != "test" && src != "input" {
            return println!("gonio: src= takes input or test.");
        }
    }
    let mut input = match Input::query(api, 1) {
        Ok(input) => input,
        Err(reason) => return println!("gonio: {reason}."),
    };
    input.default_pair();
    if let Some(pair) = value(arguments, "chan") {
        let (a, b) = pair.split_once(',').unwrap_or((pair, ""));
        let first = a.parse::<usize>().unwrap_or(0);
        let second = if b.is_empty() { first + 1 } else { b.parse::<usize>().unwrap_or(0) };
        if first == 0 || second == 0 {
            return println!("gonio: chan= takes two channel numbers, like chan=3,4.");
        }
        let second = if input.channels == 1 { first } else { second };
        if let Err(reason) = input.set_pair(first, second) {
            return println!("gonio: {reason} ({} channel(s)).", input.channels);
        }
    }
    let layout = Layout::new(width, height);
    let size = layout.scope.w as usize;
    let (Some(mut screen), Some(cells)) = (Screen::new(api, width, height).map(|screen| screen.on(started.screen)), alloc_bytes(api, size * size)) else {
        return println!("gonio: out of memory.");
    };
    let Some(mut phosphor) = Phosphor::new(cells, size) else { return println!("gonio: the screen is too small.") };
    if let Err(status) = input.start_capture(api, state.rate) {
        return println!("gonio: cannot start audio at {} Hz (code {status}); the device offers {:?}.", state.rate, &input.rates[..input.rate_count]);
    }
    if !screen.open() {
        (api.audio_stop)();
        return println!("gonio: cannot take the screen.");
    }

    phosphor.law = if value(arguments, "law") == Some("linear") { Law::Linear } else { Law::Log };
    phosphor.range_db = value(arguments, "range").and_then(parse_f32).unwrap_or(LOG_RANGE_DB).clamp(10.0, 90.0);
    let mut correlator = Correlator::new(state.rate, state.tau_s);
    let mut meters = [Channel::new(state.rate), Channel::new(state.rate)];
    let mut loudness = Loudness::new(state.rate);
    let mut panel = Panel::new();
    panel.target = match value(arguments, "target").and_then(parse_f32) {
        Some(t) => lufs::TARGETS.iter().position(|&x| x == t).unwrap_or(0),
        None => 0,
    };
    panel.wide = value(arguments, "ebu") == Some("18");
    let mut tone = PairSine::new();
    tone.set_rate(state.rate);
    tone.set_frequency(value(arguments, "tone").and_then(parse_f32).unwrap_or(997.0));
    tone.set_level_db(value(arguments, "level").and_then(parse_f32).unwrap_or(-18.0), value(arguments, "right").and_then(parse_f32).unwrap_or(0.0));
    let phase = value(arguments, "phase").and_then(parse_f32).unwrap_or(0.0);
    tone.set_phase_degrees(phase);
    let mut tone_phase = phase;
    let mut tone_us = tape_rt::micros();
    let mut tone_owed = 0.0f64;

    let block = unsafe { &mut *(&raw mut BLOCK_BYTES) };
    let left = unsafe { &mut *(&raw mut LEFT) };
    let right = unsafe { &mut *(&raw mut RIGHT) };
    let mut decoder = KeyDecoder::new();
    let mut message = Line::<48>::new();
    let mut last_draw = tape_rt::micros();
    let mut frames_drawn = 0u32;
    draw_grid(&mut screen, &layout, state.kind, &panel);

    loop {
        let keep_going = poll_keys(api, &mut decoder, |key| {
            message.clear();
            match key {
                Key::Char(b' ') => state.frozen = !state.frozen,
                Key::Char(b'r') => {
                    correlator.reset();
                    meters.iter_mut().for_each(Channel::reset);
                    phosphor.clear();
                    let _ = write!(message, "reset");
                }
                Key::Char(b'm') => {
                    state.kind = state.kind.next();
                    draw_grid(&mut screen, &layout, state.kind, &panel);
                }
                Key::Char(b'k') => {
                    let _ = write!(message, "theme {}", gfx::theme::next().name);
                    draw_grid(&mut screen, &layout, state.kind, &panel);
                }
                Key::Char(b't') => {
                    panel.target = (panel.target + 1) % lufs::TARGETS.len();
                    let _ = write!(message, "target {:.0} LUFS", panel.target_lufs());
                }
                Key::Char(b'e') => {
                    panel.wide = !panel.wide;
                    draw_grid(&mut screen, &layout, state.kind, &panel);
                }
                Key::Char(b'i') => {
                    loudness.reset();
                    panel.clear_history();
                    let _ = write!(message, "loudness reset");
                }
                Key::Char(b'v') => state.lines = !state.lines,
                Key::Char(b'w') => {
                    let mut body = tape_rt::config::Settings::<320>::new();
                    body.put("scale", format_args!("{}", if state.kind == MeterKind::Dbfs { "dbfs" } else { "nordic" }));
                    body.put("style", format_args!("{}", if state.lines { "lines" } else { "dots" }));
                    body.put("law", format_args!("{}", if phosphor.law == Law::Log { "log" } else { "linear" }));
                    body.put("range", format_args!("{:.0}", phosphor.range_db));
                    body.put("gain", format_args!("{:.0}", state.gain_db));
                    body.put("tau", format_args!("{:.2}", state.tau_s));
                    body.put("rate", format_args!("{}", state.rate));
                    body.put("target", format_args!("{:.0}", panel.target_lufs()));
                    body.put("theme", format_args!("{}", gfx::theme::theme().name));
                    body.put("ebu", format_args!("{}", if panel.wide { 18 } else { 9 }));
                    match config.save(api, body.as_str()) {
                        Ok(name) => {
                            let _ = write!(message, "saved {name}");
                        }
                        Err(reason) => {
                            let _ = write!(message, "save: {reason}");
                        }
                    }
                }
                Key::Char(b'x') => {
                    phosphor.range_db = if phosphor.range_db >= 60.0 { 30.0 } else { phosphor.range_db + 10.0 };
                    phosphor.clear();
                }
                Key::Char(b'l') => {
                    phosphor.law = phosphor.law.next();
                    phosphor.clear();
                }
                Key::Char(b'+') => state.gain_db = (state.gain_db + 3.0).min(36.0),
                Key::Char(b'-') => state.gain_db = (state.gain_db - 3.0).max(-12.0),
                Key::Char(b'p') if state.test => {
                    state.phase_index = (state.phase_index + 1) % PHASES.len();
                    tone_phase = PHASES[state.phase_index];
                    tone.set_phase_degrees(tone_phase);
                }
                Key::Char(b',') | Key::Char(b'.') if input.step_pair(key) => {
                    correlator.reset();
                    phosphor.clear();
                    let _ = write!(message, "input channels {}+{}", input.channel + 1, input.second + 1);
                }
                _ => {}
            }
            screen.mark_dirty();
        });
        if !keep_going {
            break;
        }

        let mut worked = false;
        let mut count = input.read_pair(api, &mut block[..], &mut left[..], &mut right[..], (input.channel, input.second));
        if state.test {
            let now = tape_rt::micros();
            tone_owed += now.wrapping_sub(tone_us) as f64 * state.rate as f64 / 1e6;
            tone_us = now;
            count = (tone_owed as usize).min(left.len());
            tone_owed -= count as f64;
            if count > 0 {
                tone.fill(&mut left[..count], &mut right[..count]);
            }
        }
        if count > 0 {
            loudness.feed(&left[..count], &right[..count]);
            panel.sample(&mut loudness);
            if !state.frozen {
                let (l, r) = (&left[..count], &right[..count]);
                correlator.feed(l, r);
                meters[0].feed(l);
                meters[1].feed(r);
                phosphor.trace(l, r, db_amplitude(state.gain_db), state.lines, if state.lines { 28 } else { 56 });
            }
            worked = true;
        }

        // meters decay and the phosphor fades even with no new samples, so the picture is always redrawn
        screen.mark_dirty();
        if screen.due() {
            let now = tape_rt::micros();
            let dt = (now.wrapping_sub(last_draw) as f32 / 1e6).clamp(0.001, 1.0);
            last_draw = now;
            frames_drawn += 1;
            phosphor.decay(176);
            let readings = [meters[0].read(state.kind, dt), meters[1].read(state.kind, dt)];
            let mut canvas = screen.frame();
            phosphor.draw(&mut canvas, layout.scope.x, layout.scope.y);
            let scale = state.kind.scale();
            for (channel, reading) in readings.iter().enumerate() {
                let x = layout.bar_x(channel);
                meter::draw_bar(&mut canvas, scale, x, layout.bar_w, layout.bars.y, layout.bars.h, reading.level, reading.hold, reading.clipped);
                let mut text = [0u8; 12];
                let shown = gfx::format_fixed(&mut text, reading.hold, 1);
                canvas.text_centered(x + layout.bar_w as i32 / 2, layout.bars.y + layout.bars.h as i32 + 8, if reading.hold > scale.min { shown } else { "-" }, theme().text);
                canvas.text_centered(x + layout.bar_w as i32 / 2, layout.bars.y - 26, if channel == 0 { "L" } else { "R" }, theme().dim);
            }
            panel.draw(&mut canvas, layout.left_panel, &loudness);
            let (c, b) = (layout.correlation, layout.balance);
            let correlation = correlator.correlation();
            meter::draw_bipolar(&mut canvas, c.x, c.y, c.w, c.h, correlation, true, "-1", "+1");
            meter::draw_bipolar(&mut canvas, b.x, b.y, b.w, b.h, correlator.balance(), false, "L", "R");
            let mut text = Line::<24>::new();
            let _ = write!(text, "correlation {correlation:+.2}");
            canvas.text_centered(c.x + c.w as i32 / 2, c.y - 20, text.as_str(), theme().text);
            let mut law = Line::<16>::new();
            if phosphor.law == Law::Log {
                let _ = write!(law, "log {:.0} dB", phosphor.range_db);
            } else {
                let _ = write!(law, "linear");
            }
            draw_status(&mut canvas, law.as_str(), &layout, &state, &input, tone_phase, message.as_str());
            screen.present();
        }
        if !worked {
            tape_rt::idle();
        }
    }
    (api.audio_stop)();
    screen.close();
    let mut stats = tape_rt::AudioStats::default();
    (api.audio_stats)(&mut stats);
    println!("gonio: {frames_drawn} frame(s) drawn; capture overruns {}.", stats.capture_overruns);
}

/// The static layer: background, scope graticule, meter scale, bar titles.
fn draw_grid(screen: &mut Screen, layout: &Layout, kind: MeterKind, panel: &Panel) {
    let mut canvas = screen.grid();
    canvas.clear(theme().background);
    draw_graticule(&mut canvas, layout.scope.x, layout.scope.y, layout.scope.w);
    meter::draw_scale(&mut canvas, kind.scale(), layout.bars.x, layout.bars.w, layout.bars.y, layout.bars.h);
    panel.draw_grid(&mut canvas, layout.left_panel);
    canvas.text_centered(layout.balance.x + layout.balance.w as i32 / 2, layout.balance.y - 18, "balance", theme().dim);
}

fn draw_status(canvas: &mut gfx::Canvas, law: &str, layout: &Layout, state: &State, input: &Input, phase: f32, message: &str) {
    let mut line = Line::<160>::new();
    let _ = write!(
        line,
        "GONIO  {} Hz  ch {}+{}/{}  {}  {}  {}  gain {:+.0} dB  corr {:.1} s{}{}",
        state.rate,
        input.channel + 1,
        input.second + 1,
        input.channels,
        state.kind.scale().name,
        if state.lines { "lines" } else { "dots" },
        law,
        state.gain_db,
        state.tau_s,
        if state.test { "  TEST" } else { "" },
        if state.frozen { "  FROZEN" } else { "" }
    );
    if state.test {
        let _ = write!(line, " {phase:.0} deg");
    }
    canvas.text(16, 6, line.as_str(), theme().text);
    if !message.is_empty() {
        canvas.text_right(layout.width as i32 - 16, 6, message, theme().amber);
    }
    canvas.text(16, layout.height as i32 - 18, "space freeze  r reset  m scale  v dots/lines  l law  x range  w save  k theme  t target  e EBU  i reset  +- gain  , . channels  p test phase  Esc", theme().dim);
}
