//! `ltas`: a real-time long-term average spectrum analyser for the audio device's input, drawn on the screen, with a
//! built-in noise generator for the device's output. See `tape_help!` below for the arguments and keys; the DSP is in
//! `crates/ltas` (analysis), `crates/dsp`, `crates/noise`, the drawing in `crates/gfx`. A hard-float tape: the kernel enables
//! SSE (and AVX2 where present) before it starts, and `fp_level` picks the AVX2 build of the FFT.
#![no_std]
#![no_main]

mod view;

use core::fmt::Write;
use audio_screen::{Input, Playback, Screen, poll_keys};
use gfx::Canvas;
use gfx::theme::theme;
use ltas::{Analyzer, Config, Mode, Resolution, Weighting};
use noise::{Generator, Kind as NoiseKind};
use tape_rt::{FP_AVX2, Key, KeyDecoder, Line, TapeApi, alloc_zeroed, arg_value as value, parse_f32, println, save};
use view::{Layout, Scale};

tape_rt::tape_help!(
    br#"ltas [device=N|loopN] [outdev=M|loopN] [rate=48000] [chan=1] [n=8192]
     [res=1/3|1/6|1/12|fft] [weight=z|a|c] [save=N]
     [style=smooth|bars]
     [win=hann|rect|hamming|blackman-harris|flat-top]
     [cal=DB] [tau=SECONDS|0] [simd=auto|sse2|avx2]
     [noise=off|white|pink|sine] [level=-20] [tone=HZ] [out=MASK]
     [theme=dark|classic|light]
Long-term average spectrum of the audio input, on the screen, with a test
signal generator for the output. device= is the input, outdev= a different
output device (`audio` numbers them). Curves: live, exp(onential), ltas (equal
average since reset), peak. Levels are dB re a full-scale sine.
The FFT uses AVX2+FMA when the CPU has it, else SSE2; simd= forces SSE2 (to
compare). The status line shows which; on exit the mean/worst time per frame.
Keys (Escape quits):
  space freeze   r reset   v resolution   w window   a weighting
  n FFT size     t time constant   c/x calibration +/-0.5 dB
  1 2 3 4 show live/exp/ltas/peak     Up/Down move, Left/Right scale
  g generator off/white/pink/sine   + - level   f tone   b bursts
  , . input channel -/+ (chan=N starts on N)   m smooth/bars
  k colour theme dark/classic/light (light: white, for bright rooms and paper)
  s save CSV+PNG on the first exFAT/FAT32 disk (save=N picks one)
"#
);

const MAXN: usize = 16384;
const BLOCK: usize = 32 << 10;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const SIZES: [usize; 5] = [1024, 2048, 4096, 8192, 16384];
struct AvgLabel(f32);

impl core::fmt::Display for AvgLabel {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        if self.0 <= 0.0 { write!(f, "off") } else { write!(f, "{} s", self.0) }
    }
}

const TAUS: [f32; 7] = [0.0, 0.125, 0.25, 0.5, 1.0, 2.0, 5.0];
const TONES: [f32; 7] = [125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0];

/// Four views of the most outputs there can be (the CSV writer) or one view (the drawing).
static mut LEVELS: [f32; 4 * (MAXN / 2 + 1)] = [0.0; 4 * (MAXN / 2 + 1)];
static mut BLOCK_BYTES: [u8; BLOCK] = [0; BLOCK];
static mut SAMPLES: [f32; BLOCK / 2] = [0.0; BLOCK / 2];

struct Settings {
    rate: u32,
    /// Band modes draw a spline through the band levels (`m` toggles bars).
    smooth: bool,
    resolution: Resolution,
    window: dsp::window::Kind,
    weighting: Weighting,
    n: usize,
    calibration_db: f32,
    tau_index: usize,
    avx2: bool,
}

impl Settings {
    fn config(&self) -> Config {
        let mut config = Config::new(self.rate);
        config.n = self.n;
        config.hop = self.n / 2;
        config.window = self.window;
        config.resolution = self.resolution;
        config.weighting = self.weighting;
        config.calibration_db = self.calibration_db;
        config.tau_s = TAUS[self.tau_index];
        config.avx2 = self.avx2;
        config
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let started = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(started) => started,
        Err(reason) => return println!("ltas: {reason}."),
    };
    let (arguments, width, height) = (started.arguments, started.width, started.height);
    if let Some(name) = value(arguments, "theme") {
        if !gfx::theme::set_by_name(name) {
            return println!("ltas: theme= takes dark, classic or light.");
        }
    }
    let mut settings = Settings {
        rate: value(arguments, "rate").and_then(|v| v.parse().ok()).unwrap_or(48_000),
        smooth: value(arguments, "style") != Some("bars"),
        resolution: value(arguments, "res").and_then(Resolution::from_name).unwrap_or(Resolution::Third),
        window: value(arguments, "win").and_then(dsp::window::Kind::from_name).unwrap_or(dsp::window::Kind::Hann),
        weighting: value(arguments, "weight").and_then(Weighting::from_name).unwrap_or(Weighting::Z),
        n: value(arguments, "n").and_then(|v| v.parse().ok()).filter(|n| SIZES.contains(n)).unwrap_or(8192),
        calibration_db: value(arguments, "cal").and_then(parse_f32).unwrap_or(0.0),
        tau_index: 4,
        avx2: tape_rt::fp_level() >= FP_AVX2,
    };
    match value(arguments, "simd") {
        None | Some("auto") => {}
        Some("sse2") => settings.avx2 = false,
        Some("avx2") if settings.avx2 => {}
        Some("avx2") => return println!("ltas: simd=avx2: this CPU (or the kernel) has no AVX2/FMA."),
        Some(_) => return println!("ltas: simd= takes auto, sse2 or avx2."),
    }
    if let Some(tau) = value(arguments, "tau").and_then(parse_f32) {
        settings.tau_index = (0..TAUS.len()).min_by(|&a, &b| (TAUS[a] - tau).abs().total_cmp(&(TAUS[b] - tau).abs())).unwrap_or(4);
    }
    let chan = value(arguments, "chan").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
    let mut input = match Input::query(api, chan) {
        Ok(input) => input,
        Err(reason) => return println!("ltas: {reason}."),
    };
    let playback_ok = input.playback_ok();

    let (Some(analyzer), Some(mut screen)) = (alloc_zeroed::<Analyzer<MAXN>>(api), Screen::new(api, width, height).map(|screen| screen.on(started.screen))) else {
        return println!("ltas: out of memory.");
    };
    if !analyzer.configure(settings.config()) {
        return println!("ltas: cannot configure the analyser.");
    }
    let mut generator = Generator::new();
    generator.set_rate(settings.rate);
    generator.set_level_db(value(arguments, "level").and_then(parse_f32).unwrap_or(-20.0));
    generator.set_frequency(value(arguments, "tone").and_then(parse_f32).unwrap_or(1000.0));
    let output_mask: u32 = value(arguments, "out").and_then(|v| v.parse().ok()).unwrap_or(0b11);
    let mut playback = Playback::new(api, &input, output_mask);
    let want_noise = value(arguments, "noise").and_then(NoiseKind::from_name).unwrap_or(NoiseKind::Silence);
    if want_noise != NoiseKind::Silence && !playback_ok {
        println!("ltas: the device has no outputs; the generator stays off.");
    } else {
        generator.set_kind(want_noise);
    }

    if let Err(status) = input.start(api, settings.rate) {
        return println!("ltas: cannot start audio at {} Hz (code {status}); the device offers {:?}.", settings.rate, &input.rates[..input.rate_count]);
    }
    if !screen.open() {
        (api.audio_stop)();
        return println!("ltas: cannot take the screen.");
    }
    let layout = Layout::new(width, height);
    let mut scale = Scale { db_top: 10.0, db_range: 90.0 };
    let mut shown = [false, true, true, true];
    let mut frozen = false;
    let (mut analysis_us, mut analysis_frames, mut analysis_worst) = (0u64, 0u64, 0u64);
    let mut decoder = KeyDecoder::new();
    let levels = unsafe { &mut *(&raw mut LEVELS) };
    let mut burst = false;
    let mut tone_index = 3;
    let mut message = Line::<64>::new();
    let wanted_disk = value(arguments, "save").and_then(|v| v.parse::<usize>().ok());
    let mut save_requested = false;
    let mut saves = 0u32;

    let block = unsafe { &mut *(&raw mut BLOCK_BYTES) };
    let samples = unsafe { &mut *(&raw mut SAMPLES) };
    let _ = write!(message, "ready");

    view::draw_grid(&mut screen.grid(), &layout, &scale);
    loop {
        let keep_going = poll_keys(api, &mut decoder, |key| {
            message.length = 0;
            let mut reconfigure = false;
            let mut regrid = false;
            match key {
                Key::Char(b'.') | Key::Char(b',') if input.step_channel(key) => {
                    analyzer.reset();
                    let _ = write!(message, "input channel {}", input.channel + 1);
                }
                Key::Char(b'm') => settings.smooth = !settings.smooth,
                Key::Char(b'k') => {
                    let _ = write!(message, "theme {}", gfx::theme::next().name);
                    regrid = true;
                }
                Key::Char(b's') => save_requested = true,
                Key::Char(b' ') => frozen = !frozen,
                Key::Char(b'r') => {
                    analyzer.reset();
                    let _ = write!(message, "reset");
                }
                Key::Char(b'v') => {
                    settings.resolution = settings.resolution.next();
                    reconfigure = true;
                }
                Key::Char(b'w') => {
                    settings.window = settings.window.next();
                    reconfigure = true;
                }
                Key::Char(b'a') => {
                    settings.weighting = settings.weighting.next();
                    reconfigure = true;
                }
                Key::Char(b'n') => {
                    let at = SIZES.iter().position(|&s| s == settings.n).unwrap_or(0);
                    settings.n = SIZES[(at + 1) % SIZES.len()];
                    reconfigure = true;
                }
                Key::Char(b't') => {
                    settings.tau_index = (settings.tau_index + 1) % TAUS.len();
                    reconfigure = true;
                }
                Key::Char(b'c') | Key::Char(b'x') => {
                    settings.calibration_db += if key == Key::Char(b'c') { 0.5 } else { -0.5 };
                    reconfigure = true;
                }
                Key::Char(digit @ b'1'..=b'4') => shown[(digit - b'1') as usize] = !shown[(digit - b'1') as usize],
                Key::Up => {
                    scale.db_top += 10.0;
                    regrid = true;
                }
                Key::Down => {
                    scale.db_top -= 10.0;
                    regrid = true;
                }
                Key::Left => {
                    scale.db_range = (scale.db_range - 10.0).max(30.0);
                    regrid = true;
                }
                Key::Right => {
                    scale.db_range = (scale.db_range + 10.0).min(160.0);
                    regrid = true;
                }
                Key::Char(b'g') if playback_ok => {
                    let next = generator.kind().next();
                    generator.set_kind(next);
                    let _ = write!(message, "generator {}", next.name());
                }
                Key::Char(b'+') | Key::Char(b'-') if playback_ok => {
                    let step = if key == Key::Char(b'+') { 1.0 } else { -1.0 };
                    generator.set_level_db(generator.level_db() + step);
                }
                Key::Char(b'f') => {
                    tone_index = (tone_index + 1) % TONES.len();
                    generator.set_frequency(TONES[tone_index]);
                }
                Key::Char(b'b') => {
                    burst = !burst;
                    if burst { generator.set_burst(500, 500) } else { generator.set_burst(0, 0) }
                }
                _ => {}
            }
            if reconfigure && !analyzer.configure(settings.config()) {
                let _ = write!(message, "setting not possible");
            }
            if regrid {
                view::draw_grid(&mut screen.grid(), &layout, &scale);
            }
            screen.mark_dirty();
        });
        if !keep_going {
            break;
        }

        if save_requested {
            save_requested = false;
            saves += 1;
            canvas_render(&mut screen, &layout, &scale, analyzer, shown, levels, &settings, &input, &generator, playback_ok, frozen);
            let outcome = save_measurement(api, wanted_disk, saves, analyzer, screen.pixels(), width, height, levels);
            message.length = 0;
            match outcome {
                Ok((disk, base)) => {
                    let _ = write!(message, "saved disk{disk}/{}.csv+png", base.as_str());
                }
                Err(reason) => {
                    let _ = write!(message, "save: {reason}");
                }
            }
            screen.mark_dirty();
        }

        // capture -> analysis
        let mut worked = false;
        let count = input.read(api, &mut block[..], &mut samples[..], input.channel);
        if count > 0 {
            if !frozen {
                let began = tape_rt::micros();
                let frames = analyzer.push(&samples[..count]);
                if frames > 0 {
                    let took = tape_rt::micros().wrapping_sub(began);
                    analysis_us += took;
                    analysis_frames += frames as u64;
                    analysis_worst = analysis_worst.max(took / frames as u64);
                    screen.mark_dirty();
                }
            }
            worked = true;
        }

        // generator -> playback
        if let Some(playback) = playback.as_mut() {
            worked |= playback.pump(api, &input, |mono| generator.fill(mono));
        }

        // drawing, at most 20 times a second
        if screen.due() {
            let mut canvas = screen.frame();
            view::draw_curves(&mut canvas, &layout, &scale, analyzer, shown, levels, settings.smooth);
            draw_status(&mut canvas, &layout, &settings, &input, analyzer, &generator, playback_ok, frozen, shown, message.as_str());
            screen.present();
        }
        if !worked {
            tape_rt::idle();
        }
    }
    (api.audio_stop)();
    screen.close();
    if analysis_frames > 0 {
        println!(
            "ltas: {} FFT {}: {} us per analysis on average, {} us at worst ({} frames timed).",
            if cfg!(target_arch = "aarch64") { "baseline" } else if settings.avx2 { "AVX2+FMA" } else { "SSE2" },
            settings.n,
            analysis_us / analysis_frames,
            analysis_worst,
            analysis_frames
        );
    }
    let mut stats = tape_rt::AudioStats::default();
    (api.audio_stats)(&mut stats);
    println!("ltas: {} frame(s) analysed; capture overruns {}, playback underruns {}.", analyzer.frames(), stats.capture_overruns, stats.playback_underruns);
}

/// Draws the whole picture into `frame` (no message line, so a saved screenshot is clean).
fn canvas_render<const N: usize>(screen: &mut Screen, layout: &Layout, scale: &Scale, analyzer: &mut Analyzer<N>, shown: [bool; 4], levels: &mut [f32], settings: &Settings, input: &Input, generator: &Generator, playback_ok: bool, frozen: bool) {
    let mut canvas = screen.frame();
    view::draw_curves(&mut canvas, layout, scale, analyzer, shown, levels, settings.smooth);
    draw_status(&mut canvas, layout, settings, input, analyzer, generator, playback_ok, frozen, shown, "");
}

/// Saves the CSV and the picture in `frame` as `ltas-DATE-TIME.csv` and `.png` on an exFAT/FAT32 disk.
fn save_measurement<const N: usize>(api: &TapeApi, wanted: Option<usize>, counter: u32, analyzer: &mut Analyzer<N>, frame: &[u32], width: u32, height: u32, levels: &mut [f32]) -> Result<(usize, Line<40>), &'static str> {
    let Some(disk) = save::find_disk(api, wanted) else { return Err("no exFAT or FAT32 disk (save=N picks one)") };
    let mut base = Line::<40>::new();
    let mut stamp = Line::<40>::new();
    save::names("ltas", counter, &mut base, &mut stamp);
    save::with_buffers(api, |csv_bytes, png_bytes| {
        let mut csv = save::Sink::new(csv_bytes);
        let mut png = save::Sink::new(png_bytes);
        let count = analyzer.outputs();
        ltas::csv::write(analyzer, stamp.as_str(), &mut levels[..4 * count], &mut csv).map_err(|_| "CSV too big")?;
        let mut full = true;
        let drawn = gfx::png::write_rgb(&frame[..(width * height) as usize], width, height, width, |chunk| full &= png.push(chunk));
        if !drawn || !full {
            return Err("PNG too big");
        }
        if save::write_file(api, disk, base.as_str(), "csv", csv.written()) != 0 || save::write_file(api, disk, base.as_str(), "png", png.written()) != 0 {
            return Err("cannot write (disk full or read-only?)");
        }
        Ok((disk, base))
    })
    .unwrap_or(Err("out of memory"))
}

fn draw_status<const N: usize>(canvas: &mut Canvas, layout: &Layout, settings: &Settings, input: &Input, analyzer: &Analyzer<N>, generator: &Generator, playback_ok: bool, frozen: bool, shown: [bool; 4], message: &str) {
    let mut line = Line::<200>::new();
    let _ = write!(
        line,
        "LTAS  {} Hz  ch {}/{}  {}{}  FFT {} {} {}  weight {}  cal {:+.1} dB  avg {}  frames {}{}",
        settings.rate,
        input.channel + 1,
        input.channels,
        settings.resolution.name(),
        if settings.resolution == Resolution::Fft { "" } else if settings.smooth { " smooth" } else { " bars" },
        settings.n,
        if settings.avx2 { "avx2" } else if cfg!(target_arch = "x86_64") { "sse2" } else { "base" },
        settings.window.name(),
        settings.weighting.name(),
        settings.calibration_db,
        AvgLabel(TAUS[settings.tau_index]),
        analyzer.frames(),
        if frozen { "  FROZEN" } else { "" }
    );
    canvas.text(layout.left, 6, line.as_str(), theme().text);
    let mut x = layout.left;
    for (index, mode) in Mode::ALL.into_iter().enumerate() {
        let colour = if shown[index] { theme().curves[index] } else { theme().dim };
        x = canvas.text(x, 24, mode.name(), colour) + 16;
    }
    if playback_ok {
        let mut text = Line::<80>::new();
        let _ = write!(text, "generator: {}", generator.kind().name());
        if generator.kind() != NoiseKind::Silence {
            let _ = write!(text, " {:+.0} dB", generator.level_db());
            if generator.kind() == NoiseKind::Sine {
                let _ = write!(text, " {:.0} Hz", generator.frequency());
            }
        }
        canvas.text(x + 16, 24, text.as_str(), theme().text);
    }
    canvas.text(layout.left, layout.height as i32 - 18, "space freeze  r reset  v res  m smooth  w win  a weight  n size  t avg  , . chan  s save  g gen  +- lvl  f tone  b burst  Esc", theme().dim);
    if !message.is_empty() {
        canvas.text_right(layout.left + layout.plot_w as i32, 6, message, theme().curves[1]);
    }
}
