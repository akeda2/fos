//! KakelFix: a mixer, recorder and analyzer for the active audio device, driven by mouse, touch or keyboard. The Mixer
//! page drives the device's hardware mixer (tape ABI 41, `mixer_*`); the Analyzer page captures a channel pair and shows
//! its waveform and spectrum (`analyze.rs`); the Meters page meters every input with its gain under it (`meters.rs`); the
//! Recorder comes later. The state, the actions and the layout are in
//! `crates/kakelfix` (host-tested); this file is the device glue, the input loop and `kakelfix show`; `view.rs` draws.
#![no_std]
#![no_main]

mod analyze;
mod meters;
mod view;

use audio_screen::{Screen, poll_keys};
use fosfor_abi::tape::{
    MIXER_CONTROL_ACTION, MIXER_CONTROL_BOOL, MIXER_FLAG_UNKNOWN, MIXER_HAS_MASTERS, MIXER_HAS_METERS, MixerControl, MixerMeter,
};
use instrument_rt::{TouchTarget, Touches};
use core::fmt::Write;
use core::sync::atomic::Ordering;
use kakelfix::{Action, App, Device, Grab, Layout, Link, Page, Queued, Shape, View};
use mixer_model::{Group, lin_to_db};
use tape_rt::mixer::Mixer;
use tape_rt::{Key, KeyDecoder, TapeApi, print, println};

tape_rt::tape_help!(
    br#"kakelfix [device=N] [screen=N] [theme=NAME] [rate=HZ] [show]
The active audio device's mixer on the pixel screen: one output's mix as
fader strips (Outputs), every send as a table (Matrix) and the device's own
settings (Device); the Analyzer page shows an input pair's waveform and
spectrum; the Meters page meters every input (dBFS, Nordic PPM or LUFS)
with its gain and switches under it. Mouse, touch or keyboard.
  device=N     use audio device N (`audio` lists them; loop0 for a try)
  screen=N     which screen (Pi 4: 1 is the second HDMI output)
  theme=NAME   dark, classic or light
  rate=HZ      the Analyzer's capture rate (default 48000 if offered)
  ask-phantom=yes  ask before switching phantom power on (Device page's
               last row; kakelfix.cfg keeps it)
kakelfix.cfg also keeps the Analyzer's settings, saved as they change:
an-view=waveform|spectrum|both an-wave=envelope|scope an-span=1|5|30
an-timebase=5|10|20|50 an-scale=db|linear an-pair=N
an-resolution=third|sixth|twelfth|fft, and the Meters page's
mt-scale=dbfs|nordic|lufs mt-input=N (the same words on the command line)
  show         print every output's sends and the controls, then end
Keys: Tab next view, 1-4 Mixer/Recorder/Analyzer/Meters, arrows select and
move (Up/Down: 1 dB), PgUp/PgDn 6 dB, + - 0.1 dB, 0 unity, x off, m mute,
, . pan, c centre, o O next/previous output, p pair source, P pair output,
L pan law, Enter open (Matrix) or change (Device), y n answer, Esc quits.
Analyzer: Tab view, Left/Right or , . input pair, Up/Down span, space
freeze, w envelope/scope, d dB/linear, f resolution, r reset.
Meters: Left/Right input, Up/Down or + - gain, h +48V, i other switch,
Tab or s scale, r reset (holds, clip, loudness).
Mouse: click a strip, drag its fader or pan, click soft keys and cells.
"#
);

const MAX_WIDTH: u32 = 3840;
const MAX_HEIGHT: u32 = 2160;
/// Meters the tape reads per frame: inputs, playback channels, outputs.
pub const METERS: usize = 3 * mixer_model::MAX_CHANNELS;

/// The active device's mixer as the `Device` the app drives.
pub struct Hardware {
    pub mixer: Mixer,
}

impl Device for Hardware {
    fn gain(&self, output: usize, source: usize) -> Option<u32> {
        self.mixer.get(output as u32, source as u32)
    }
    fn set_gain(&mut self, output: usize, source: usize, raw: u32) -> bool {
        self.mixer.set(output as u32, source as u32, raw)
    }
    fn master(&self, output: usize) -> Option<u32> {
        self.mixer.master(output as u32)
    }
    fn set_master(&mut self, output: usize, raw: u32) -> bool {
        self.mixer.set_master(output as u32, raw)
    }
    fn serial(&self) -> u64 {
        self.mixer.serial()
    }
    fn name(&self, group: Group, index: usize, out: &mut [u8]) -> usize {
        self.mixer.name(group.idx() as u32, index as u32, out).len()
    }
    fn control(&self, index: usize) -> Option<MixerControl> {
        self.mixer.control(index as u32)
    }
    fn control_item(&self, index: usize, item: usize, out: &mut [u8]) -> usize {
        self.mixer.control_item(index as u32, item as u32, out).len()
    }
    fn set_control(&mut self, index: usize, value: i64) -> bool {
        self.mixer.control_set(index as u32, value)
    }
}

/// Mixer writes waiting for the worker thread (`kakelfix::queue`).
static LINK: Link = Link::new();

/// The worker: writes what the drawing thread queued, sleeping on `LINK.work` in between; at the end it finishes the
/// queue. `argument` points at the tape's `Mixer` (a copy is enough: the handle is the call table).
extern "C" fn writer(argument: usize) {
    let mixer = unsafe { *(argument as *const Mixer) };
    let mut hardware = Hardware { mixer };
    loop {
        let seen = LINK.work.load(Ordering::Acquire);
        if !LINK.drain(&mut hardware) {
            if LINK.stopping() {
                break;
            }
            tape_rt::futex_wait(&LINK.work, seen);
        }
    }
}

/// The device as the drawing thread sees it.
pub type Shadowed = Queued<'static, Hardware>;

/// The mouse and fingers act on the app through this: a press on a fader or pan control holds it until release.
struct Pointing<'a> {
    app: &'a mut App,
    hardware: &'a mut Shadowed,
    layout: &'a Layout,
}

impl TouchTarget for Pointing<'_> {
    type Grab = Grab;
    fn down(&mut self, x: i32, y: i32) -> Option<Grab> {
        self.app.scroll(self.layout);
        self.app.press(self.hardware, self.layout, x, y)
    }
    fn moved(&mut self, grab: Grab, x: i32, y: i32) -> Grab {
        self.app.drag(self.hardware, self.layout, grab, x, y);
        grab
    }
    fn up(&mut self, _grab: Grab) {}
}

fn shape(mixer: &Mixer) -> Shape {
    let info = mixer.info;
    Shape {
        counts: [info.inputs as usize, info.playbacks as usize, info.outputs as usize],
        unity: info.unity,
        max: info.max,
        master_max: info.master_max,
        masters: info.flags & MIXER_HAS_MASTERS != 0,
        controls: info.controls as usize,
    }
}

fn action(key: Key) -> Option<Action> {
    Some(match key {
        Key::Tab => Action::NextView,
        Key::Up => Action::Up,
        Key::Down => Action::Down,
        Key::Left => Action::Left,
        Key::Right => Action::Right,
        Key::Home => Action::Home,
        Key::End => Action::End,
        Key::PageUp => Action::Step(6.0),
        Key::PageDown => Action::Step(-6.0),
        Key::Enter => Action::Enter,
        Key::Char(b'1') => Action::Page(Page::Mixer),
        Key::Char(b'2') => Action::Page(Page::Recorder),
        Key::Char(b'3') => Action::Page(Page::Analyzer),
        Key::Char(b'4') => Action::Page(Page::Meters),
        Key::Char(b's') => Action::MeterScale,
        Key::Char(b'h') => Action::Phantom,
        Key::Char(b'i') => Action::InputSwitch,
        Key::Char(b'+') | Key::Char(b'=') => Action::Step(0.1),
        Key::Char(b'-') => Action::Step(-0.1),
        Key::Char(b'0') => Action::Unity,
        Key::Char(b'x') => Action::Off,
        Key::Char(b'm') => Action::Mute,
        Key::Char(b',') => Action::Pan(-0.1),
        Key::Char(b'.') => Action::Pan(0.1),
        Key::Char(b'c') => Action::Center,
        Key::Char(b'o') => Action::NextOutput,
        Key::Char(b'O') => Action::PrevOutput,
        Key::Char(b'p') => Action::Pair,
        Key::Char(b'P') => Action::PairOutput,
        Key::Char(b'L') => Action::PanLaw,
        Key::Char(b'y') => Action::Yes,
        Key::Char(b'n') => Action::No,
        Key::Char(b'q') => Action::Quit,
        Key::Char(b'v') => Action::View(View::Outputs),
        Key::Char(b' ') => Action::Freeze,
        Key::Char(b'w') => Action::WaveMode,
        Key::Char(b'd') => Action::WaveScale,
        Key::Char(b'f') => Action::Resolution,
        Key::Char(b'r') => Action::Reset,
        _ => return None,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn tape_main(api: &'static TapeApi) {
    tape_rt::init(api);
    let arguments = match tape_rt::take_audio_device_arg(api, core::str::from_utf8(tape_rt::args()).unwrap_or("")) {
        Ok(arguments) => arguments,
        Err(reason) => return println!("kakelfix: {reason}."),
    };
    let Some(mixer) = Mixer::open() else {
        return println!("kakelfix: the audio device has no mixer (`audio device N`; loop0 has one to try).");
    };
    if arguments.split_whitespace().any(|word| word == "show") {
        return show(&mixer);
    }
    let started = match audio_screen::startup(api, MAX_WIDTH, MAX_HEIGHT) {
        Ok(started) => started,
        Err(reason) => return println!("kakelfix: {reason}."),
    };
    if let Some(name) = tape_rt::arg_value(started.arguments, "theme") {
        if !gfx::theme::set_by_name(name) {
            return println!("kakelfix: theme= takes dark, classic or light.");
        }
    }
    // `ask-phantom=yes|no` (command line or kakelfix.cfg): whether switching phantom power on asks first. Off by default.
    let config = match tape_rt::config::Config::load(api, "kakelfix", started.arguments) {
        Ok(config) => config,
        Err(reason) => return println!("kakelfix: {reason}."),
    };
    let (width, height) = (started.width, started.height);
    let Some(mut screen) = Screen::new(api, width, height).map(|screen| screen.on(started.screen)) else {
        return println!("kakelfix: out of memory.");
    };
    if !screen.open() {
        return println!("kakelfix: cannot open the screen.");
    }
    let layout = Layout::new(width, height);
    let has_meters = mixer.info.flags & MIXER_HAS_METERS != 0;
    // Where the kernel runs tape threads, writes go to a worker so a slow device (milliseconds per mixer write) does not
    // hold up the screen; otherwise they are made inline.
    let mut writer_thread = None;
    if tape_rt::threads_supported() {
        match tape_rt::spawn_thread(writer, &mixer as *const Mixer as usize, 64 * 1024) {
            Ok(thread) => writer_thread = Some(thread),
            Err(code) => println!("kakelfix: no writer thread ({code}); writing inline."),
        }
    }
    let info = mixer.info;
    let link = writer_thread.as_ref().map(|_| &LINK);
    let mut hardware: Shadowed = Queued::new(Hardware { mixer }, link, info.outputs as usize, (info.inputs + info.playbacks) as usize);
    let mut app = App::new(shape(&mixer), &mut hardware);
    app.ask_hazard = config.value("ask-phantom") == Some("yes");
    let rate = tape_rt::arg_value(started.arguments, "rate").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut capture = analyze::Capture::new(api, rate);
    app.analysis.channels = capture.channels().max(1);
    app.analysis.load(|key| config.value(key));
    app.analysis.pair = app.analysis.pair.min(app.analysis.pairs() - 1);
    app.metering.channels = capture.channels().max(1);
    app.metering.load(|key| config.value(key));
    let persisted = |app: &App| (app.analysis.persisted(), app.metering.persisted());
    let mut saved = persisted(&app);
    // When the Analyzer's or the Meters page's settings last changed; they are written a second later, so a run of key repeats is one write.
    let mut changed_at = None;
    let mut failures = 0;
    let mut woken = LINK.work.load(Ordering::Acquire);
    let mut touches = Touches::<Grab>::new(width as i32, height as i32, started.screen);
    let mut meters = view::Meters::new();
    let mut readings = [MixerMeter::default(); METERS];
    let mut decoder = KeyDecoder::new();
    let mut last_tick = 0u64;
    let mut last_draw = tape_rt::micros();
    screen.grid().clear(gfx::theme::theme().background);

    loop {
        let mut keys = [None; 16];
        let mut count = 0;
        let keep_going = poll_keys(api, &mut decoder, |key| {
            if count < keys.len() {
                keys[count] = action(key);
                count += 1;
            }
        });
        if !keep_going {
            // Through the quit check below, so a pending save is written first.
            app.quit = true;
        }
        for action in keys[..count].iter().flatten() {
            app.act(&mut hardware, *action);
            screen.mark_dirty();
        }
        let mut pointing = Pointing { app: &mut app, hardware: &mut hardware, layout: &layout };
        if touches.poll(&mut pointing) {
            screen.mark_dirty();
        }
        if persisted(&app) != saved && changed_at.is_none() {
            changed_at = Some(tape_rt::micros());
        }
        let settled = changed_at.is_some_and(|at| app.quit || tape_rt::micros().wrapping_sub(at) >= 1_000_000);
        if app.ask_changed || settled {
            changed_at = None;
            saved = persisted(&app);
            let mut body = tape_rt::config::Settings::<512>::new();
            body.put("ask-phantom", format_args!("{}", if app.ask_hazard { "yes" } else { "no" }));
            app.analysis.save(|key, value| body.put(key, value));
            app.metering.save(|key, value| body.put(key, value));
            let result = config.save(api, body.as_str());
            // The Analyzer and Meters save quietly; the +48 V toggle and any failure say so.
            if app.ask_changed || result.is_err() {
                app.message.clear();
                let _ = match result {
                    Ok(name) => write!(app.message, "saved {name}"),
                    Err(reason) => write!(app.message, "save: {reason}"),
                };
                screen.mark_dirty();
            }
            app.ask_changed = false;
        }
        if app.quit {
            break;
        }
        let work = LINK.work.load(Ordering::Acquire);
        if work != woken {
            woken = work;
            tape_rt::futex_wake(&LINK.work, 1);
        }
        if LINK.failures() != failures {
            failures = LINK.failures();
            hardware.invalidate();
            app.refused(&mut hardware);
            screen.mark_dirty();
        }
        let now = tape_rt::micros();
        if now.wrapping_sub(last_tick) >= 100_000 {
            last_tick = now;
            if app.tick(&mut hardware) {
                screen.mark_dirty();
            }
        }
        let fresh = capture.update(api, &mut app, &layout);
        if has_meters || fresh {
            screen.mark_dirty();
        }
        if screen.due() {
            let now = tape_rt::micros();
            let seconds = (now.wrapping_sub(last_draw) as f32 / 1e6).clamp(0.001, 1.0);
            last_draw = now;
            if has_meters {
                let read = hardware.device.mixer.meters(&mut readings);
                meters.update(&readings[..read], seconds);
            }
            app.scroll(&layout);
            let mut canvas = screen.frame();
            view::draw(&mut canvas, &layout, &app, &hardware, &meters, &mut capture, touches.pointer());
            screen.present();
        } else {
            tape_rt::idle();
        }
    }
    capture.stop(api, &mut app);
    screen.close();
    if let Some(thread) = writer_thread {
        LINK.stop();
        tape_rt::futex_wake(&LINK.work, 1);
        thread.join();
    }
}

/// `kakelfix show`: every output's non-zero sends and its master, then the controls, as text.
fn show(mixer: &Mixer) {
    let info = mixer.info;
    println!("kakelfix: {} ({} in, {} play, {} out)", mixer.title(), info.inputs, info.playbacks, info.outputs);
    let mut names = [0u8; 32];
    for output in 0..info.outputs {
        print!("{:>8}:", mixer.name(2, output, &mut names));
        if info.flags & MIXER_HAS_MASTERS != 0 {
            print!(" master {}", Level(mixer.master(output), info.unity));
        }
        let mut column = 22;
        for source in 0..mixer.sources() {
            let raw = mixer.get(output, source);
            if raw == Some(0) {
                continue;
            }
            let (group, index) = if source < info.inputs { (0, source) } else { (1, source - info.inputs) };
            let name = mixer.name(group, index, &mut names);
            if column + name.len() + 8 > 78 {
                print!("\n         ");
                column = 9;
            }
            print!(" {name} {}", Level(raw, info.unity));
            column += name.len() + 8;
        }
        println!();
    }
    for index in 0..info.controls {
        let Some(control) = mixer.control(index) else { continue };
        let mut item = [0u8; 32];
        let text = mixer.control_item(index, control.value as u32, &mut item);
        let name = kakelfix::field(&control.name);
        if control.flags & MIXER_FLAG_UNKNOWN != 0 && control.kind != MIXER_CONTROL_ACTION {
            println!("  {name}: ?");
        } else if control.kind == MIXER_CONTROL_BOOL {
            println!("  {name}: {}", if control.value != 0 { "On" } else { "Off" });
        } else if text.is_empty() {
            println!("  {name}: {}", control.value);
        } else {
            println!("  {name}: {text}");
        }
    }
}

struct Level(Option<u32>, u32);

impl core::fmt::Display for Level {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            None => out.write_str("?"),
            Some(0) => out.write_str("off"),
            Some(raw) => {
                let db = kakelfix::format_db(lin_to_db(raw as f64 / self.1 as f64), false, false);
                out.write_str(db.as_str())
            }
        }
    }
}
