//! `ltas`: a real-time long-term average spectrum analyser for the audio device's input, drawn on the screen, with a
//! built-in noise generator for the device's output. See `tape_help!` below for the arguments and keys; the DSP is in
//! `crates/ltas` (analysis), `crates/dsp`, `crates/noise`, the drawing in `crates/gfx`. A hard-float tape: the kernel enables
//! SSE (and AVX2 where present) before it starts, and `fp_level` picks the AVX2 build of the FFT.
#![no_std]
#![no_main]

mod view;

use core::fmt::Write;
use dsp::pcm::unpack_channel;
use gfx::Canvas;
use ltas::{Analyzer, Config, Mode, Resolution, Weighting};
use noise::{Generator, Kind as NoiseKind};
use tape_rt::{AudioInfo, FP_AVX2, Key, KeyDecoder, TapeApi, println};
use view::{Layout, Scale};

tape_rt::tape_help!(
    br#"ltas [device=N] [rate=48000] [chan=1] [res=1/3|1/6|1/12|fft] [n=8192]
     [win=hann|rect|hamming|blackman-harris|flat-top] [weight=z|a|c]
     [cal=DB] [tau=SECONDS] [noise=off|white|pink|sine] [level=-20]
     [tone=HZ] [out=MASK]
Long-term average spectrum of the audio input, on the screen, with a test
signal generator for the output. Curves: live, exp(onential), ltas (equal
average since reset), peak. Levels are dB re a full-scale sine.
Keys (Escape quits):
  space freeze   r reset   v resolution   w window   a weighting
  n FFT size     t time constant   c/x calibration +/-0.5 dB
  1 2 3 4 show live/exp/ltas/peak     Up/Down move, Left/Right scale
  g generator off/white/pink/sine   + - level   f tone   b bursts
"#
);

const MAXN: usize = 16384;
const BLOCK: usize = 32 << 10;
const MAX_WIDTH: u32 = 1920;
const MAX_HEIGHT: u32 = 1080;
const SIZES: [usize; 5] = [1024, 2048, 4096, 8192, 16384];
const TAUS: [f32; 6] = [0.125, 0.25, 0.5, 1.0, 2.0, 5.0];
const TONES: [f32; 7] = [125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0];

static mut BLOCK_BYTES: [u8; BLOCK] = [0; BLOCK];
static mut SAMPLES: [f32; BLOCK / 2] = [0.0; BLOCK / 2];
static mut NOISE_MONO: [f32; 4096] = [0.0; 4096];
static mut NOISE_BYTES: [u8; 4096 * 4 * 24] = [0; 4096 * 4 * 24];

/// A line of text for the canvas, built with `write!`.
struct Line<const N: usize> {
    bytes: [u8; N],
    length: usize,
}

impl<const N: usize> Line<N> {
    fn new() -> Self {
        Self { bytes: [0; N], length: 0 }
    }
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }
}

impl<const N: usize> Write for Line<N> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        for byte in text.bytes() {
            if self.length < N {
                self.bytes[self.length] = byte;
                self.length += 1;
            }
        }
        Ok(())
    }
}

struct Settings {
    rate: u32,
    channel: usize,
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

fn alloc_zeroed<T>(api: &TapeApi) -> Option<&'static mut T> {
    let bytes = core::mem::size_of::<T>();
    let address = (api.alloc)(bytes);
    if address == 0 {
        return None;
    }
    unsafe {
        core::ptr::write_bytes(address as *mut u8, 0, bytes);
        Some(&mut *(address as *mut T))
    }
}

fn alloc_pixels(api: &TapeApi, count: usize) -> Option<&'static mut [u32]> {
    let address = (api.alloc)(count * 4);
    if address == 0 {
        return None;
    }
    Some(unsafe { core::slice::from_raw_parts_mut(address as *mut u32, count) })
}

fn value<'a>(arguments: &'a str, key: &str) -> Option<&'a str> {
    arguments.split_ascii_whitespace().find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}

fn parse_f32(text: &str) -> Option<f32> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let mut result = whole.parse::<u32>().ok()? as f32;
    let mut scale = 0.1;
    for digit in fraction.bytes() {
        if !digit.is_ascii_digit() {
            return None;
        }
        result += (digit - b'0') as f32 * scale;
        scale /= 10.0;
    }
    Some(if negative { -result } else { result })
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(rest) => rest,
        Err(reason) => return println!("ltas: {reason}."),
    };
    if tape_rt::fp_level() == 0 {
        return println!("ltas: needs a hard-float tape build (kernel without FP support?).");
    }
    let Some((width, height)) = tape_rt::gfx_info() else {
        return println!("ltas: no pixel screen (BIOS text mode). Use a UEFI boot or a VBE mode.");
    };
    let (width, height) = (width.min(MAX_WIDTH), height.min(MAX_HEIGHT));
    let mut settings = Settings {
        rate: value(arguments, "rate").and_then(|v| v.parse().ok()).unwrap_or(48_000),
        channel: value(arguments, "chan").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1) - 1,
        resolution: value(arguments, "res").and_then(Resolution::from_name).unwrap_or(Resolution::Third),
        window: value(arguments, "win").and_then(dsp::window::Kind::from_name).unwrap_or(dsp::window::Kind::Hann),
        weighting: value(arguments, "weight").and_then(Weighting::from_name).unwrap_or(Weighting::Z),
        n: value(arguments, "n").and_then(|v| v.parse().ok()).filter(|n| SIZES.contains(n)).unwrap_or(8192),
        calibration_db: value(arguments, "cal").and_then(parse_f32).unwrap_or(0.0),
        tau_index: 3,
        avx2: tape_rt::fp_level() >= FP_AVX2,
    };
    if let Some(tau) = value(arguments, "tau").and_then(parse_f32) {
        settings.tau_index = (0..TAUS.len()).min_by(|&a, &b| (TAUS[a] - tau).abs().total_cmp(&(TAUS[b] - tau).abs())).unwrap_or(3);
    }
    let mut info = AudioInfo::default();
    if (api.audio_info)(&mut info) == 0 || info.capture_channels == 0 {
        return println!("ltas: no audio input (USB audio or HD Audio with a capture side).");
    }
    if settings.channel >= info.capture_channels as usize {
        return println!("ltas: the device has {} input channel(s); chan={} is out of range.", info.capture_channels, settings.channel + 1);
    }
    let (capture_channels, capture_slot, capture_bits) = (info.capture_channels as usize, info.capture_slot_bytes as usize, info.capture_bits as usize);
    let playback_ok = info.playback_channels > 0 && info.playback_slot_bytes > 0;

    let (Some(analyzer), Some(grid), Some(frame)) = (
        alloc_zeroed::<Analyzer<MAXN>>(api),
        alloc_pixels(api, (width * height) as usize),
        alloc_pixels(api, (width * height) as usize),
    ) else {
        return println!("ltas: out of memory.");
    };
    if !analyzer.configure(settings.config()) {
        return println!("ltas: cannot configure the analyser.");
    }
    let mut generator = Generator::new();
    generator.set_rate(settings.rate);
    generator.set_level_db(value(arguments, "level").and_then(parse_f32).unwrap_or(-20.0));
    generator.set_frequency(value(arguments, "tone").and_then(parse_f32).unwrap_or(1000.0));
    let mut output_mask: u32 = value(arguments, "out").and_then(|v| v.parse().ok()).unwrap_or(0b11);
    if output_mask == 0 {
        output_mask = 0b11;
    }
    let want_noise = value(arguments, "noise").and_then(NoiseKind::from_name).unwrap_or(NoiseKind::Silence);
    if want_noise != NoiseKind::Silence && !playback_ok {
        println!("ltas: the device has no outputs; the generator stays off.");
    } else {
        generator.set_kind(want_noise);
    }

    let status = (api.audio_start)(settings.rate, 1, playback_ok as u32);
    if status != 0 {
        return println!("ltas: cannot start audio at {} Hz (code {status}); the device offers {:?}.", settings.rate, &info.rates[..info.rate_count as usize]);
    }
    if !tape_rt::gfx_open() {
        (api.audio_stop)();
        return println!("ltas: cannot take the screen.");
    }
    let layout = Layout::new(width, height);
    let mut scale = Scale { db_top: 10.0, db_range: 90.0 };
    let mut shown = [false, true, true, true];
    let mut frozen = false;
    let mut decoder = KeyDecoder::new();
    let mut levels = [0.0f32; MAXN];
    let mut dirty = true;
    let mut next_draw = 0u64;
    let mut burst = false;
    let mut tone_index = 3;
    let mut message = Line::<64>::new();

    let block = unsafe { &mut *(&raw mut BLOCK_BYTES) };
    let samples = unsafe { &mut *(&raw mut SAMPLES) };
    let mono = unsafe { &mut *(&raw mut NOISE_MONO) };
    let packed = unsafe { &mut *(&raw mut NOISE_BYTES) };
    let capture_frame = capture_channels * capture_slot;
    let playback_frame = info.playback_channels as usize * info.playback_slot_bytes as usize;
    let _ = write!(message, "ready");

    {
        let mut canvas = Canvas::new(grid, width, height).unwrap();
        view::draw_grid(&mut canvas, &layout, &scale);
    }
    'main: loop {
        // keys
        loop {
            let event = (api.read_key)();
            if event == 0 {
                break;
            }
            if event == 1 {
                break 'main;
            }
            let Some(key) = decoder.feed((event & 0xff) as u8) else { continue };
            message.length = 0;
            let mut reconfigure = false;
            let mut regrid = false;
            match key {
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
                let mut canvas = Canvas::new(grid, width, height).unwrap();
                view::draw_grid(&mut canvas, &layout, &scale);
            }
            dirty = true;
        }

        // capture -> analysis
        let mut worked = false;
        let got = (api.audio_read)(block.as_mut_ptr(), BLOCK / capture_frame * capture_frame);
        if got >= capture_frame {
            let count = unpack_channel(&block[..got], &mut samples[..], capture_channels, capture_slot, capture_bits, settings.channel);
            if !frozen && analyzer.push(&samples[..count]) > 0 {
                dirty = true;
            }
            worked = true;
        }

        // generator -> playback
        if playback_ok {
            let mut now = AudioInfo::default();
            (api.audio_info)(&mut now);
            let room = now.playback_space as usize / playback_frame.max(1);
            let frames = room.min(mono.len()).min(packed.len() / playback_frame.max(1));
            if frames > 0 {
                generator.fill(&mut mono[..frames]);
                let bytes = noise::pack(&mono[..frames], &mut packed[..], info.playback_channels as usize, info.playback_slot_bytes as usize, info.playback_bits as usize, output_mask);
                (api.audio_write)(packed.as_ptr(), bytes);
                worked = true;
            }
        }

        // drawing, at most 20 times a second
        let now_us = tape_rt::micros();
        if dirty && now_us >= next_draw {
            next_draw = now_us + 50_000;
            dirty = false;
            let mut canvas = Canvas::new(frame, width, height).unwrap();
            canvas.pixels[..(width * height) as usize].copy_from_slice(&grid[..(width * height) as usize]);
            view::draw_curves(&mut canvas, &layout, &scale, analyzer, shown, &mut levels);
            draw_status(&mut canvas, &layout, &settings, analyzer, &generator, playback_ok, frozen, shown, message.as_str());
            tape_rt::gfx_blit(0, 0, width, height, &canvas.pixels[..(width * height) as usize], width);
        }
        if !worked {
            tape_rt::idle();
        }
    }
    (api.audio_stop)();
    tape_rt::gfx_close();
    let mut stats = tape_rt::AudioStats::default();
    (api.audio_stats)(&mut stats);
    println!("ltas: {} frame(s) analysed; capture overruns {}, playback underruns {}.", analyzer.frames(), stats.capture_overruns, stats.playback_underruns);
}

fn draw_status<const N: usize>(canvas: &mut Canvas, layout: &Layout, settings: &Settings, analyzer: &Analyzer<N>, generator: &Generator, playback_ok: bool, frozen: bool, shown: [bool; 4], message: &str) {
    let mut line = Line::<200>::new();
    let _ = write!(
        line,
        "LTAS  {} Hz  {}  FFT {} {}  weight {}  cal {:+.1} dB  avg {} s  frames {}{}",
        settings.rate,
        settings.resolution.name(),
        settings.n,
        settings.window.name(),
        settings.weighting.name(),
        settings.calibration_db,
        TAUS[settings.tau_index],
        analyzer.frames(),
        if frozen { "  FROZEN" } else { "" }
    );
    canvas.text(layout.left, 6, line.as_str(), view::TEXT);
    let mut x = layout.left;
    for (index, mode) in Mode::ALL.into_iter().enumerate() {
        let colour = if shown[index] { view::COLOURS[index] } else { view::DIM };
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
        canvas.text(x + 16, 24, text.as_str(), view::TEXT);
    }
    canvas.text(layout.left, layout.height as i32 - 18, "space freeze  r reset  v res  w window  a weight  n size  t avg  g gen  Esc quit", view::DIM);
    if !message.is_empty() {
        canvas.text_right(layout.left + layout.plot_w as i32, 6, message, view::EXP);
    }
}
