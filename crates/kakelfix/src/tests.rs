use super::*;
use fosfor_abi::tape::MIXER_CONTROL_INT;

const UNITY: u32 = 0x8000;

/// Two inputs, two playback channels, two outputs; input crosspoints unknown until written (like a device that cannot
/// read them back); a "Dim" switch and a hazardous "Phantom" switch.
pub(crate) struct Fake {
    pub gain: [[Option<u32>; 4]; 2],
    master: [u32; 2],
    pub controls: [i64; 3],
    serial: u64,
    pub writes: usize,
    pub refuse: bool,
    /// "Trim" reports `MIXER_FLAG_UNKNOWN`.
    pub unknown_trim: bool,
    /// The three controls' names: a switch, a hazardous switch, a number.
    pub names: [&'static str; 3],
}

impl Fake {
    pub fn new() -> Fake {
        Fake { gain: [[None, None, Some(UNITY), Some(0)], [None, None, Some(0), Some(UNITY)]], master: [UNITY; 2], controls: [0; 3], serial: 1, writes: 0, refuse: false, unknown_trim: false, names: ["Dim", "Phantom", "Trim"] }
    }
}

impl Device for Fake {
    fn gain(&self, o: usize, s: usize) -> Option<u32> {
        self.gain[o][s]
    }
    fn set_gain(&mut self, o: usize, s: usize, raw: u32) -> bool {
        if self.refuse {
            return false;
        }
        self.gain[o][s] = Some(raw);
        self.serial += 1;
        self.writes += 1;
        true
    }
    fn master(&self, o: usize) -> Option<u32> {
        Some(self.master[o])
    }
    fn set_master(&mut self, o: usize, raw: u32) -> bool {
        self.master[o] = raw;
        self.serial += 1;
        self.writes += 1;
        true
    }
    fn serial(&self) -> u64 {
        self.serial
    }
    fn name(&self, group: Group, index: usize, out: &mut [u8]) -> usize {
        let name = match (group, index) {
            (Group::Playback, 0) => "Play L",
            (Group::Playback, 1) => "Play R",
            (Group::Output, 0) => "Out L",
            (Group::Output, 1) => "Out R",
            _ => return 0,
        };
        out[..name.len()].copy_from_slice(name.as_bytes());
        name.len()
    }
    fn control(&self, index: usize) -> Option<MixerControl> {
        let mut control = MixerControl::default();
        let name: &str = match index {
            0 => self.names[0],
            1 => {
                control.flags = MIXER_FLAG_HAZARD;
                self.names[1]
            }
            2 => {
                control.kind = MIXER_CONTROL_INT;
                control.min = -10;
                control.max = 10;
                if self.unknown_trim {
                    control.flags = fosfor_abi::tape::MIXER_FLAG_UNKNOWN;
                }
                self.names[2]
            }
            _ => return None,
        };
        if index < 2 {
            control.kind = MIXER_CONTROL_BOOL;
            control.max = 1;
        }
        control.value = self.controls[index];
        control.name[..name.len()].copy_from_slice(name.as_bytes());
        Some(control)
    }
    fn control_item(&self, _: usize, _: usize, _: &mut [u8]) -> usize {
        0
    }
    fn set_control(&mut self, index: usize, value: i64) -> bool {
        self.controls[index] = value;
        self.serial += 1;
        true
    }
}

fn app(device: &mut Fake) -> App {
    App::new(Shape { counts: [2, 2, 2], unity: UNITY, max: 0x10000, master_max: 0x10000, masters: true, controls: 3 }, device)
}

#[test]
fn opens_with_pairs_labels_and_unknown_inputs() {
    let mut device = Fake::new();
    let app = app(&mut device);
    let sources = app.sources();
    assert_eq!(sources.len(), 2);
    assert_eq!(app.label(&sources[0]).as_str(), "In 1/2");
    assert_eq!(app.label(&sources[1]).as_str(), "Play L/R");
    assert_eq!(app.label(&app.output()).as_str(), "Out L/R");
    assert_eq!(app.strip_count(), 3);
    let (db, _, _, unknown, _) = app.strip_level(0);
    assert!(unknown && !db.is_finite());
    let (db, ..) = app.strip_level(1);
    assert_eq!(db, 0.0);
    assert_eq!(app.controls.len(), 3);
}

#[test]
fn keys_edit_the_selected_send_and_master() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Down);
    assert_eq!(device.gain[0][2], Some(29_205), "-1 dB on the playback pair");
    app.act(&mut device, Action::Off);
    assert_eq!((device.gain[0][2], device.gain[1][3]), (Some(0), Some(0)));
    app.act(&mut device, Action::Unity);
    app.act(&mut device, Action::Mute);
    assert_eq!(device.gain[0][2], Some(0));
    app.act(&mut device, Action::End);
    app.act(&mut device, Action::Step(-6.0));
    assert_eq!(device.master, [16_423; 2]);
    // Our own writes do not count as someone else's change.
    assert!(!app.tick(&mut device));
}

#[test]
fn a_change_made_elsewhere_is_read_back() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    device.set_gain(0, 0, UNITY);
    device.set_gain(1, 1, UNITY);
    assert!(app.tick(&mut device));
    let (db, _, _, unknown, _) = app.strip_level(0);
    assert!(!unknown && db == 0.0);
}

#[test]
fn hazardous_controls_ask_first() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.act(&mut device, Action::View(View::Device));
    app.act(&mut device, Action::Enter);
    assert_eq!(device.controls[0], 1, "Dim toggles at once");
    app.act(&mut device, Action::Down);
    app.act(&mut device, Action::Enter);
    assert_eq!((device.controls[1], app.confirm), (1, None), "Phantom switches at once while asking is off");
    app.act(&mut device, Action::Enter);
    assert_eq!(device.controls[1], 0);
    for _ in 0..2 {
        app.act(&mut device, Action::Down);
    }
    assert_eq!(app.ctl_sel, app.ask_row());
    app.act(&mut device, Action::Enter);
    assert!(app.ask_hazard && app.ask_changed, "the row after the device's controls switches asking on");
    for _ in 0..2 {
        app.act(&mut device, Action::Up);
    }
    app.act(&mut device, Action::Enter);
    assert_eq!((device.controls[1], app.confirm), (0, Some((1, 1))), "Phantom waits for a yes");
    app.act(&mut device, Action::No);
    assert_eq!((device.controls[1], app.confirm), (0, None));
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Yes);
    assert_eq!(device.controls[1], 1);
    app.act(&mut device, Action::Down);
    app.act(&mut device, Action::Left);
    assert_eq!(app.control_value(&device, 2).as_str(), "-1");
    device.unknown_trim = true;
    app.rederive(&mut device);
    assert_eq!(app.control_value(&device, 2).as_str(), "?", "a value the device does not know");
}

#[test]
fn pairing_splits_and_rereads_without_writing() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Pair);
    assert_eq!(app.message.as_str(), "Play L split");
    assert_eq!(app.strip_count(), 4);
    assert_eq!(app.label(&app.sources()[1]).as_str(), "Play L");
    assert_eq!(device.writes, 0);
}

#[test]
fn pairing_inputs_follows_and_sets_the_device_link() {
    let mut device = Fake::new();
    device.names[1] = "Link 1/2";
    let mut app = app(&mut device);
    assert!(!app.mix.paired[Group::Input.idx()][0], "the device's link (off) decides how the inputs start");
    app.act(&mut device, Action::Pair);
    assert_eq!((app.message.as_str(), device.controls[1]), ("In 1 paired", 1));
    app.act(&mut device, Action::Pair);
    assert_eq!((app.message.as_str(), device.controls[1]), ("In 1 split", 0));
    app.ask_hazard = true;
    app.act(&mut device, Action::Pair);
    assert_eq!((app.confirm, device.controls[1]), (Some((1, 1)), 0), "linking a hazardous pair asks first");
    // Playback pairs have no device link.
    app.confirm = None;
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Pair);
    assert_eq!((app.message.as_str(), device.controls[1]), ("Play L split", 0));
}

#[test]
fn the_mouse_drags_faders_and_pan_and_presses_soft_keys() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    let layout = Layout::new(1280, 720);
    app.scroll(&layout);
    let fader = layout.fader(1, &app).unwrap();
    let grab = app.press(&mut device, &layout, fader.x + 2, fader.y);
    assert!(matches!(grab, Some(Grab::Pressed { strip: 1, .. })));
    assert_eq!(device.gain[0][2], Some(UNITY), "a press alone leaves the fader");
    app.release(&mut device, &layout, grab.unwrap());
    assert_eq!(device.gain[0][2], Some(65_381), "a tap sets it: the top is +6 dB");
    app.drag(&mut device, &layout, Grab::Fader(1), fader.x + 2, fader.y + fader.h);
    assert_eq!(device.gain[0][2], Some(0), "the bottom is off");
    app.drag(&mut device, &layout, Grab::Fader(1), fader.x + 2, fader.y + fader.h - (fader.h as f64 * layout::fader_position(0.0)) as i32);
    assert_eq!(app.strip_level(1).0, 0.0);
    let pan = layout.pan(1, &app).unwrap();
    assert_eq!(app.press(&mut device, &layout, pan.x, pan.y + 1), Some(Grab::Pan(1)));
    assert_eq!(device.gain[1][3], Some(0), "hard left on a pair turns the right side off");
    let matrix = layout.soft(&app, 2);
    assert_eq!(app.press(&mut device, &layout, matrix.x + 1, matrix.y + 1), None);
    assert_eq!(app.view, View::Matrix);
    app.scroll(&layout);
    let cell = layout.cell(1, 0, &app).unwrap();
    let grab = app.press(&mut device, &layout, cell.x + 1, cell.y + 1).unwrap();
    app.release(&mut device, &layout, grab);
    assert_eq!((app.row, app.col, app.view), (1, 0, View::Matrix));
    let grab = app.press(&mut device, &layout, cell.x + 1, cell.y + 1).unwrap();
    app.release(&mut device, &layout, grab);
    assert_eq!((app.src_sel, app.view), (1, View::Outputs), "a second click opens the send");
}

#[test]
fn a_finger_swipes_the_strips_the_matrix_and_the_meters() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    // A narrow screen shows fewer strips than there are.
    let layout = Layout::new(320, 720);
    app.scroll(&layout);
    let (visible, count) = (layout.visible_strips(), app.strip_count());
    assert!(visible < count, "{visible} of {count}");
    // Sideways from a fader: the strips follow the finger and no level changes.
    let fader = layout.fader(0, &app).unwrap();
    let (x, y) = (fader.x + 2, fader.y + fader.h / 2);
    let mut grab = app.press(&mut device, &layout, x, y).unwrap();
    for step in 1..=4 {
        grab = app.drag(&mut device, &layout, grab, x - step * layout.strip_w / 2, y + 1);
    }
    assert_eq!(app.first_strip, (count - visible).min(2), "two strips' width to the left shows two more");
    assert!(app.src_sel >= app.first_strip, "the selection stays in view");
    app.release(&mut device, &layout, grab);
    app.scroll(&layout);
    assert_eq!(app.first_strip, (count - visible).min(2), "`scroll` keeps it there");
    assert_eq!(device.writes, 0, "a swipe writes nothing");
    // Back to the right, past the start: clamped.
    let label = layout.label(app.first_strip, &app).unwrap();
    let grab = app.press(&mut device, &layout, label.x + 2, label.y + 2).unwrap();
    app.drag(&mut device, &layout, grab, label.x + 2 + 10 * layout.strip_w, label.y + 2);
    assert_eq!(app.first_strip, 0);
    // A swipe that starts on the selected matrix cell does not open it (a tap would).
    app.act(&mut device, Action::View(View::Matrix));
    app.scroll(&layout);
    assert_eq!((app.row, app.col), (0, 0));
    let cell = layout.cell(0, 0, &app).unwrap();
    let grab = app.press(&mut device, &layout, cell.x + 1, cell.y + 1).unwrap();
    let grab = app.drag(&mut device, &layout, grab, cell.x + 1 - layout.cell_w, cell.y + 1);
    app.release(&mut device, &layout, grab);
    assert_eq!(app.view, View::Matrix);
    // Meters: more inputs than columns; a swipe to the left shows later inputs.
    app.metering.channels = 30;
    app.act(&mut device, Action::Page(Page::Meters));
    app.scroll(&layout);
    assert!(layout.visible_meters(&app) < 30);
    let bar = layout.meter_bar(0, &app).unwrap();
    let grab = app.press(&mut device, &layout, bar.x + 2, bar.y + 4).unwrap();
    app.drag(&mut device, &layout, grab, bar.x + 2 - 3 * layout.meter_column_w(&app), bar.y + 4);
    assert_eq!((app.first_meter, app.metering.input), (3, 3));
}

#[test]
fn the_fader_scale_round_trips() {
    for db in [6.0, 0.0, -6.5, -12.0, -30.0, -60.0] {
        assert_eq!(layout::fader_db(layout::fader_position(db)), db);
    }
    assert!(!layout::fader_db(0.0).is_finite());
    assert_eq!(layout::fader_position(f64::NEG_INFINITY), 0.0);
}

#[test]
fn scrolling_keeps_the_selection_visible() {
    assert_eq!(keep(0, 0, 4, 10), 0);
    assert_eq!(keep(5, 0, 4, 10), 2);
    assert_eq!(keep(3, 2, 4, 10), 2);
    assert_eq!(keep(1, 2, 4, 10), 1);
    assert_eq!(keep(9, 0, 20, 10), 0);
}

#[test]
fn levels_and_pan_format() {
    assert_eq!(format_db(-6.04, false, false).as_str(), "-6.0");
    assert_eq!(format_db(0.0, false, false).as_str(), "+0.0");
    assert_eq!(format_db(f64::NEG_INFINITY, false, false).as_str(), "off");
    assert_eq!(format_db(0.0, true, false).as_str(), "mute");
    assert_eq!(format_db(0.0, false, true).as_str(), "?");
    assert_eq!(format_pan(0.0).as_str(), "C");
    assert_eq!(format_pan(-0.5).as_str(), "L50");
    assert_eq!(format_pan(0.25).as_str(), "R25");
}

#[test]
fn soft_keys_follow_the_page_and_tab_cycles_its_views() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    let layout = Layout::new(1280, 720);
    let labels = |app: &App| app.soft_keys().map(|k| k.label);
    assert_eq!(labels(&app), ["Mixer", "Outputs", "Matrix", "Device", "Recorder", "Analyzer", "Meters", "Pair", "Pan law", "", "Quit"]);
    let analyzer = layout.soft(&app, 5);
    app.press(&mut device, &layout, analyzer.x + 1, analyzer.y + 1);
    assert_eq!(app.page, Page::Analyzer);
    assert_eq!(labels(&app), ["Mixer", "Recorder", "Analyzer", "Waveform", "Spectrum", "Both", "Meters", "Freeze", "Wave: Env", "Reset", "Quit"]);
    assert!(layout.soft(&app, 3).x > layout.soft(&app, 2).x, "views are indented");
    app.act(&mut device, Action::NextView);
    assert_eq!((app.page, app.analysis.view), (Page::Analyzer, AnView::Waveform));
    let spectrum = layout.soft(&app, 4);
    app.press(&mut device, &layout, spectrum.x + 1, spectrum.y + 1);
    assert_eq!(app.analysis.view, AnView::Spectrum);
    let empty = layout.soft(&app, 9);
    let quit = layout.soft(&app, 10);
    assert!(quit.y > empty.y + empty.h, "a gap before Quit");
    app.act(&mut device, Action::Page(Page::Recorder));
    assert_eq!(labels(&app), ["Mixer", "Recorder", "Analyzer", "Meters", "", "", "", "Record", "Stop", "Project", "Quit"]);
    let gap = layout.soft(&app, 5);
    app.press(&mut device, &layout, gap.x + 1, gap.y + 1);
    assert!(!app.quit, "an empty slot does nothing");
    app.press(&mut device, &layout, quit.x + 1, quit.y + 1);
    assert!(app.quit);
}

#[test]
fn analyzer_keys_step_pair_and_span_and_restart_the_buffers() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.act(&mut device, Action::Page(Page::Analyzer));
    app.analysis.channels = 5;
    let start = app.analysis.generation;
    app.act(&mut device, Action::Left);
    assert_eq!((app.analysis.pair, app.analysis.channels_shown()), (2, (4, 4)), "wraps; an odd last channel shows twice");
    app.act(&mut device, Action::Pan(0.1));
    assert_eq!(app.analysis.channels_shown(), (0, 1));
    app.act(&mut device, Action::Up);
    app.act(&mut device, Action::Up);
    assert_eq!(app.analysis.span, 2, "stops at the longest");
    assert_eq!(app.analysis.generation, start.wrapping_add(3));
    app.act(&mut device, Action::WaveMode);
    app.act(&mut device, Action::Down);
    assert_eq!((app.analysis.wave, app.analysis.timebase, app.analysis.span), (WaveMode::Scope, 0, 2));
    app.act(&mut device, Action::Freeze);
    assert!(app.analysis.frozen && app.soft_keys()[7].active);
    app.act(&mut device, Action::Mute);
    assert_eq!(device.writes, 0, "mixer keys do nothing on the Analyzer page");
}

#[test]
fn the_envelope_keeps_min_and_max_per_column_newest_last() {
    let mut envelope = analyzer::Envelope::new();
    envelope.configure(4, 1000, 0.012);
    assert_eq!(envelope.filled(), 0);
    let left = [0.1, -0.5, 0.2, 0.9, 0.0, 0.0, -0.1, 0.3, 0.4];
    let right = [0.0; 9];
    envelope.push(&left[..5], &right[..5]);
    envelope.push(&left[5..], &right[5..]);
    assert_eq!(envelope.filled(), 3, "three samples a column");
    assert_eq!(envelope.back(0)[0], [-0.1, 0.4]);
    assert_eq!(envelope.back(2)[0], [-0.5, 0.2]);
    envelope.push(&[0.0; 6], &[1.0; 6]);
    assert_eq!(envelope.filled(), 4, "at most the column count");
    assert_eq!(envelope.back(0)[1], [1.0, 1.0]);
    assert_eq!(envelope.back(3)[0], [0.0, 0.9], "the oldest kept is the second");
}

#[test]
fn the_scope_starts_on_a_rising_zero_crossing_or_free_runs() {
    let mut scope = analyzer::Scope::new();
    scope.configure(1000, 4.0);
    assert!(scope.trace(0).0.is_empty());
    let wave: [f32; 12] = [0.5, 0.2, -0.3, -0.6, -0.1, 0.4, 0.8, 0.3, -0.2, 0.0, 0.0, 0.0];
    scope.push(&wave, &wave);
    let (trace, triggered) = scope.trace(0);
    assert!(triggered);
    assert_eq!(trace, &[0.4, 0.8, 0.3, -0.2]);
    let mut silent = analyzer::Scope::new();
    silent.configure(1000, 4.0);
    silent.push(&[0.0; 20], &[0.0; 20]);
    assert_eq!(silent.trace(1), (&[0.0f32; 4][..], false));
}

#[test]
fn analyzer_settings_are_saved_and_loaded_back() {
    let mut a = Analysis::new();
    a.view = AnView::Spectrum;
    a.wave = WaveMode::Scope;
    a.span = 2;
    a.timebase = 3;
    a.db = true;
    a.resolution = 3;
    a.pair = 1;
    a.frozen = true;
    let mut text = Text::<256>::new();
    a.save(|key, value| {
        let _ = write!(text, "{key}={value} ");
    });
    let text = text.as_str();
    let mut b = Analysis::new();
    b.load(|key| text.split(' ').find_map(|word| word.strip_prefix(key)?.strip_prefix('=')));
    assert_eq!(b.persisted(), a.persisted());
    assert!(!b.frozen, "Freeze is not a setting");
    let mut c = Analysis::new();
    c.load(|key| (key == "an-view").then_some("sideways"));
    assert_eq!(c.persisted(), Analysis::new().persisted(), "an unknown word changes nothing");
}

#[test]
fn meter_controls_belong_to_the_input_their_number_names() {
    assert_eq!(meters::input_of("Mic gain 3"), Some(3));
    assert_eq!(meters::input_of("+48V 12"), Some(12));
    assert_eq!(meters::input_of("Level 5"), Some(5));
    assert_eq!(meters::input_of("Loopback 1"), None, "not an input's control");
    assert_eq!(meters::input_of("Clock"), None);
    assert_eq!(meters::short_label("+48V 1"), "+48V");
    let mut device = Fake::new();
    device.names = ["Inst 2", "+48V 1", "Mic gain 1"];
    let mut app = app(&mut device);
    app.metering.channels = 2;
    assert_eq!(&app.input_controls(0)[..], &[1, 2]);
    assert_eq!(&app.input_controls(1)[..], &[0]);
    assert_eq!(app.meter_rows(), 2);
    assert_eq!(app.gain_control(0), Some(2));
    assert_eq!(app.gain_control(1), None);
    assert_eq!(app.control_short(&device, 2).as_str(), "0");
    assert_eq!(app.control_short(&device, 1).as_str(), "+48V");
}

#[test]
fn meters_keys_select_inputs_change_gain_and_ask_for_phantom() {
    let mut device = Fake::new();
    device.names = ["Inst 2", "+48V 1", "Mic gain 1"];
    let mut app = app(&mut device);
    app.metering.channels = 2;
    app.act(&mut device, Action::Page(Page::Meters));
    assert_eq!(app.soft_keys()[3].label, "Meters");
    assert_eq!(app.soft_keys()[7].label, "Scale: PPM");
    app.act(&mut device, Action::Up);
    app.act(&mut device, Action::Step(6.0));
    assert_eq!(device.controls[2], 7, "Up and PgUp step the selected input's gain");
    app.act(&mut device, Action::Step(30.0));
    assert_eq!(device.controls[2], 10, "clamped at the control's maximum");
    app.ask_hazard = true;
    app.act(&mut device, Action::Phantom);
    assert_eq!((device.controls[1], app.confirm), (0, Some((1, 1))), "+48V asks first");
    app.act(&mut device, Action::Yes);
    assert_eq!(device.controls[1], 1);
    app.act(&mut device, Action::Right);
    assert_eq!(app.metering.input, 1);
    app.act(&mut device, Action::Right);
    assert_eq!(app.metering.input, 1, "stays on the last input");
    app.act(&mut device, Action::Up);
    assert!(app.message.as_str().contains("no gain"), "{}", app.message.as_str());
    app.act(&mut device, Action::InputSwitch);
    assert_eq!(device.controls[0], 1, "i flips input 2's Inst switch");
    app.act(&mut device, Action::NextView);
    assert_eq!(app.metering.scale, MeterScale::Lufs);
    assert_eq!((app.metering.bars(), app.metering.bar()), (1, 0));
    let generation = app.metering.generation;
    app.act(&mut device, Action::Reset);
    assert_ne!(app.metering.generation, generation);
}

#[test]
fn meters_mouse_selects_and_drags_the_gain() {
    let mut device = Fake::new();
    device.names = ["Inst 2", "+48V 1", "Mic gain 1"];
    let mut app = app(&mut device);
    app.metering.channels = 2;
    app.act(&mut device, Action::Page(Page::Meters));
    let layout = Layout::new(1280, 800);
    app.scroll(&layout);
    let bar = layout.meter_bar(1, &app).unwrap();
    app.press(&mut device, &layout, bar.x + 2, bar.y + 10);
    assert_eq!(app.metering.input, 1);
    let gain = layout.meter_cell(0, 1, &app).unwrap();
    let grab = app.press(&mut device, &layout, gain.x + 2, gain.y + 2);
    assert_eq!(app.metering.input, 0);
    let grab = grab.expect("a gain cell is dragged");
    app.drag(&mut device, &layout, grab, gain.x + 2, gain.y + 2 - layout.row * 2);
    assert_eq!(device.controls[2], 4, "two rows up is four steps");
    app.drag(&mut device, &layout, grab, gain.x + 2, gain.y + 2 + layout.row * 20);
    assert_eq!(device.controls[2], -10);
    let inst = layout.meter_cell(1, 0, &app).unwrap();
    assert!(app.press(&mut device, &layout, inst.x + 2, inst.y + 2).is_none());
    assert_eq!(device.controls[0], 1, "a click flips a switch");
    // In LUFS a column is a pair: its right half selects the right input.
    app.act(&mut device, Action::MeterScale);
    let pair = layout.meter_bar(0, &app).unwrap();
    app.press(&mut device, &layout, pair.x + pair.w - 4, pair.y + 10);
    assert_eq!(app.metering.input, 1);
}

#[test]
fn meter_settings_load_and_save() {
    let mut m = Metering::new();
    m.channels = 4;
    m.load(|key| match key {
        "mt-scale" => Some("lufs"),
        "mt-input" => Some("9"),
        _ => None,
    });
    assert_eq!((m.scale, m.input), (MeterScale::Lufs, 3), "the input is clamped to what is captured");
    let mut text = Text::<64>::new();
    m.save(|key, value| {
        let _ = write!(text, "{key}={value} ");
    });
    assert_eq!(text.as_str(), "mt-scale=lufs mt-input=4 mt-show=all ");
}

#[test]
fn meters_show_only_the_chosen_inputs() {
    let mut m = Metering::new();
    m.channels = 8;
    m.scale = MeterScale::Dbfs;
    m.input = 2;
    m.load(|key| (key == "mt-show").then_some("2,4-5,40"));
    assert_eq!((m.bars(), m.input), (3, 3), "the selection moves to the next shown input");
    assert_eq!((m.bar_input(0), m.bar_input(2), m.bar_input(3), m.bar_of(4), m.bar_of(0)), (Some(1), Some(4), None, Some(2), None));
    m.step(true);
    assert_eq!(m.input, 4);
    m.step(true);
    assert_eq!(m.input, 4, "stays on the last shown input");
    m.home(false);
    assert_eq!(m.input, 1);
    // LUFS: a pair has a bar when either input is shown.
    m.scale = MeterScale::Lufs;
    assert_eq!((m.bars(), m.bar_input(1), m.bar_of(3)), (3, Some(2), Some(1)));
    let mut text = Text::<64>::new();
    m.save(|key, value| {
        if key == "mt-show" {
            let _ = write!(text, "{value}");
        }
    });
    assert_eq!(text.as_str(), "2,4-5");
    m.show(0);
    assert_eq!(m.bars(), 4, "showing nothing shows everything");
}

#[test]
fn names_keep_their_number_when_cut() {
    assert_eq!(fit_name("Mic 10", 6).as_str(), "Mic 10");
    assert_eq!(fit_name("Mic 10", 5).as_str(), "Mic10");
    assert_eq!(fit_name("ADAT 12", 5).as_str(), "AD 12");
    assert_eq!(fit_name("Analog 1", 4).as_str(), "An 1");
    assert_eq!(fit_name("Mic 10", 3).as_str(), "M10");
    assert_eq!(fit_name("Playback", 4).as_str(), "Play");
    assert_eq!(fit_name("Mic 10", 2).as_str(), "Mi", "no room for the number");
}

#[test]
fn the_meters_picker_chooses_the_inputs_shown() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.metering.channels = 8;
    app.act(&mut device, Action::Page(Page::Meters));
    let layout = Layout::new(1280, 800);
    app.act(&mut device, Action::Channels);
    assert_eq!(app.picking, Some(0));
    let groups = app.meter_groups().len();
    assert_eq!(app.meter_groups()[0].0.as_str(), "All");
    // A tap on input 3 hides it; a tap on every other input but 1 leaves 1, which then stays.
    let three = layout.pick(groups + 2, &app).unwrap();
    assert_eq!(app.press(&mut device, &layout, three.x + 2, three.y + 2), None);
    assert!(!app.metering.is_shown(2) && app.metering.is_shown(3));
    for input in (1..8).filter(|&i| i != 2) {
        let cell = layout.pick(groups + input, &app).unwrap();
        app.press(&mut device, &layout, cell.x + 2, cell.y + 2);
    }
    assert_eq!(app.metering.bars(), 1);
    let first = layout.pick(groups, &app).unwrap();
    app.press(&mut device, &layout, first.x + 2, first.y + 2);
    assert!(app.metering.is_shown(0), "the last input shown stays");
    // Keys: Right to input 2, Enter shows it; the "All" preset, then Done closes.
    app.picking = Some(groups);
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Enter);
    assert_eq!(app.metering.bars(), 2);
    app.act(&mut device, Action::Home);
    app.picking = Some(0);
    app.act(&mut device, Action::Freeze);
    assert_eq!(app.metering.bars(), 8);
    let done = layout.pick(app.pick_items() - 1, &app).unwrap();
    app.press(&mut device, &layout, done.x + 2, done.y + 2);
    assert_eq!(app.picking, None);
    // A page change closes it too.
    app.act(&mut device, Action::Channels);
    app.act(&mut device, Action::Page(Page::Mixer));
    assert_eq!(app.picking, None);
    assert_eq!(device.writes, 0);
}

#[test]
fn analyzer_pair_buttons_tap_and_swipe() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.analysis.channels = 30;
    app.act(&mut device, Action::Page(Page::Analyzer));
    let layout = Layout::new(640, 480);
    app.scroll(&layout);
    assert!(layout.visible_pairs() < 15);
    let third = layout.an_pair(2, &app).unwrap();
    let grab = app.press(&mut device, &layout, third.x + 2, third.y + 2).unwrap();
    app.release(&mut device, &layout, grab);
    assert_eq!(app.analysis.pair, 2, "a tap shows that pair");
    // A swipe to the left reveals later pairs and changes nothing else.
    let grab = app.press(&mut device, &layout, third.x + 2, third.y + 2).unwrap();
    let grab = app.drag(&mut device, &layout, grab, third.x + 2 - 3 * layout.an_pair_w(), third.y + 2);
    app.release(&mut device, &layout, grab);
    assert_eq!((app.first_pair, app.analysis.pair), (3, 2));
    // `scroll` brings the shown pair back into view when a key changes it.
    app.act(&mut device, Action::Left);
    app.scroll(&layout);
    assert!(app.first_pair <= app.analysis.pair);
}

#[test]
fn the_swipe_band_scrolls_without_touching_a_fader() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    let layout = Layout::new(320, 720);
    app.scroll(&layout);
    let band = layout.swipe_band();
    assert!(band.h > layout.row && band.y > layout.strips.y + layout.strips.h);
    let (x, y) = (band.x + band.w - 4, band.y + band.h / 2);
    let grab = app.press(&mut device, &layout, x, y).unwrap();
    let grab = app.drag(&mut device, &layout, grab, x - layout.strip_w, y);
    app.release(&mut device, &layout, grab);
    assert_eq!(app.first_strip, 1);
    // A tap there does nothing at all.
    let before = (app.src_sel, app.first_strip);
    if let Some(grab) = app.press(&mut device, &layout, x, y) {
        app.release(&mut device, &layout, grab);
    }
    assert_eq!((app.src_sel, app.first_strip, device.writes), (before.0, before.1, 0));
}

#[test]
fn a_held_finger_flips_the_inputs_key_once() {
    use instrument_rt::{TouchTarget, Touches};
    use tape_rt::TouchEvent;
    struct Pointing<'a> {
        app: &'a mut App,
        device: &'a mut Fake,
        layout: &'a Layout,
    }
    impl TouchTarget for Pointing<'_> {
        type Grab = Grab;
        fn down(&mut self, x: i32, y: i32) -> Option<Grab> {
            Some(self.app.press(self.device, self.layout, x, y).unwrap_or(Grab::Held))
        }
        fn moved(&mut self, grab: Grab, x: i32, y: i32) -> Grab {
            self.app.drag(self.device, self.layout, grab, x, y)
        }
        fn up(&mut self, grab: Grab) {
            self.app.release(self.device, self.layout, grab);
        }
    }
    let mut device = Fake::new();
    let mut app = app(&mut device);
    app.metering.channels = 4;
    app.act(&mut device, Action::Page(Page::Meters));
    app.act(&mut device, Action::Channels);
    let layout = Layout::new(1280, 800);
    let slot = app.soft_keys().iter().position(|k| k.action == Action::Channels).unwrap();
    let key = layout.soft(&app, slot);
    let mut touches = Touches::<Grab>::new(1280, 800, 0);
    let mut target = Pointing { app: &mut app, device: &mut device, layout: &layout };
    // A touch panel reports a resting finger again and again before it lifts.
    for down in [true, true, true, true, false] {
        let event = TouchEvent { panel: 0, id: 1, x: key.x + 4, y: key.y + 4, down, screen: -1 };
        touches.handle(&event, (1280, 800), &mut target);
    }
    assert_eq!(app.picking, None, "closed once, not reopened");
}

#[test]
fn looped_back_inputs_are_named_after_their_output() {
    let mut device = Fake::new();
    device.names = ["Loopback 3", "Phantom", "Trim"];
    let mut app = app(&mut device);
    app.metering.channels = 16;
    app.metering.load(|key| (key == "mt-loop").then_some("13-14:1,x,16:9"));
    assert_eq!((app.input_name(12).as_str(), app.input_name(13).as_str(), app.input_name(15).as_str()), ("Loop 1", "Loop 2", "Loop 9"));
    assert_eq!(app.input_name(2).as_str(), "In 3", "the device's loopback switch is off");
    app.act(&mut device, Action::View(View::Device));
    app.act(&mut device, Action::Enter);
    app.controls = (0..3).filter_map(|i| device.control(i)).collect();
    assert_eq!(app.input_name(2).as_str(), "Loop 3", "on: capture channel 3 carries output 3");
    let loops = app.meter_groups().iter().find(|(name, _)| name.as_str() == "Loop").map(|g| g.1);
    assert_eq!(loops, Some(1 << 2 | 1 << 12 | 1 << 13 | 1 << 15), "a preset for the loops");
    let mut text = Text::<64>::new();
    app.metering.save(|key, value| {
        if key == "mt-loop" {
            let _ = write!(text, "{value}");
        }
    });
    assert_eq!(text.as_str(), "13-14:1,16:9");
}

fn disk(number: usize, volume: recorder::Volume, boot: bool, largest: u64) -> recorder::Disk {
    recorder::Disk { number, volume, boot, largest, free: largest, ..recorder::Disk::EMPTY }
}

/// A recorder app with four inputs and three disks (the boot FOSFS disk, an ext4 disk, an exFAT stick).
fn recorder_app(device: &mut Fake) -> App {
    let mut app = app(device);
    let r = &mut app.recording;
    r.channels = 4;
    r.disks[0] = disk(1, recorder::Volume::Fosfs, true, 1 << 30);
    r.disks[1] = recorder::Disk { writable: true, ..disk(2, recorder::Volume::Ext, false, 1 << 30) };
    r.disks[2] = disk(3, recorder::Volume::Exfat, false, 10 << 30);
    r.disk_count = 3;
    r.fix();
    app.act(device, Action::Page(Page::Recorder));
    app
}

#[test]
fn recorder_prefers_exfat_then_ext_and_never_fosfs_or_fat32() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    let r = &mut app.recording;
    r.choose_disk(None);
    assert_eq!(r.target().unwrap().name().as_str(), "disk3");
    r.choose_disk(Some("ext2"));
    assert_eq!(r.target().unwrap().name().as_str(), "ext2", "the saved volume wins");
    // FOSFS is listed but never chosen, not even when it was saved (or named by an old bare number).
    r.choose_disk(Some("1"));
    assert_eq!(r.target().unwrap().name().as_str(), "disk3");
    r.disk = 0;
    assert_eq!(r.refusal(), Some("FOSFS is not used for recordings: exFAT or ext4"));
    r.disks[2].volume = recorder::Volume::None;
    r.choose_disk(None);
    assert_eq!(r.target().unwrap().name().as_str(), "ext2");
    r.disk = 2;
    assert_eq!(r.refusal(), Some("that disk has no volume (format it first)"));
    r.disks[2].volume = recorder::Volume::Fat32;
    r.choose_disk(Some("disk3"));
    assert_eq!(r.target().unwrap().name().as_str(), "ext2");
    r.disk = 2;
    assert_eq!(r.refusal(), Some("FAT32 is not used for recordings: exFAT or ext4"));
    // Neither exFAT nor ext: the boot disk is shown, and refused.
    r.disks[1].volume = recorder::Volume::Fosfs;
    r.choose_disk(None);
    assert_eq!((r.target().unwrap().number, r.refusal()), (1, Some("FOSFS is not used for recordings: exFAT or ext4")));
}

#[test]
fn recorder_flac_groups_eight_inputs_a_file() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    let r = &mut app.recording;
    r.channels = 30;
    r.armed = (1 << 30) - 1;
    let (groups, count) = r.groups();
    assert_eq!(count, 4, "30 inputs are four files");
    assert_eq!(groups[..4], [0xff, 0xff00, 0xff_0000, 0x3f00_0000]);
    // Gaps: the 8 armed inputs, not 8 numbers, make a group.
    r.armed = 0b1010_1010_1010_1010_1010;
    let (groups, count) = r.groups();
    assert_eq!((count, groups[0], groups[1]), (2, 0b1010_1010_1010_1010, 0b1010_0000_0000_0000_0000));
    // Up to 8: one file. WAV: always one file, the device's samples.
    r.armed = 0b11;
    assert_eq!(r.groups().1, 1);
    r.armed = (1 << 30) - 1;
    app.act(&mut device, Action::Resolution);
    app.act(&mut device, Action::Resolution);
    app.act(&mut device, Action::Resolution);
    let r = &app.recording;
    assert_eq!(r.encoding, recorder::Encoding::Wav);
    assert_eq!(r.groups().1, 1);
    assert_eq!(r.extension(), "wav");
    app.recording.take.running = true;
    app.act(&mut device, Action::Resolution);
    assert_eq!(app.message.as_str(), "stop the take to change the format");
}

#[test]
fn recorder_arms_inputs_and_keeps_one() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    assert_eq!(app.recording.armed & app.recording.all(), 0b11, "the default stays within four inputs");
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Enter);
    assert!(app.recording.is_armed(2));
    app.act(&mut device, Action::Home);
    app.act(&mut device, Action::Enter);
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Enter);
    assert_eq!(app.recording.armed, 0b100);
    app.act(&mut device, Action::Right);
    app.act(&mut device, Action::Enter);
    assert_eq!(app.recording.armed, 0b100, "the last armed input stays");
    assert_eq!(app.message.as_str(), "a take needs at least one input");
    app.act(&mut device, Action::Channels);
    assert_eq!(app.recording.armed_count(), 4);
    // FLAC 24 is costed uncompressed: 3 bytes a sample.
    assert_eq!(app.recording.bytes_per_second(), 48_000 * 3 * 4);
}

#[test]
fn recorder_takes_lock_the_arming_and_the_disk() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    app.recording.choose_disk(None);
    app.act(&mut device, Action::Freeze);
    assert_eq!(app.recording.request, Some(recorder::Request::Start));
    // The tape takes the request and reports the take running.
    app.recording.request = None;
    app.recording.take.running = true;
    assert!(app.soft_keys()[SOFT_NAV].active, "Record is lit");
    app.act(&mut device, Action::Enter);
    assert_eq!(app.message.as_str(), "stop the take to change what it records");
    app.act(&mut device, Action::NextDisk);
    assert_eq!(app.message.as_str(), "stop the take to change the disk");
    // Another page: R again leaves it running, S stops it.
    app.act(&mut device, Action::Page(Page::Analyzer));
    app.act(&mut device, Action::Record);
    assert_eq!(app.recording.request, None);
    app.act(&mut device, Action::Stop);
    assert_eq!(app.recording.request, Some(recorder::Request::Stop));
}

#[test]
fn the_sample_rate_is_a_device_setting_and_stays_put_during_a_take() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    app.rates[..4].copy_from_slice(&[44_100, 48_000, 96_000, 192_000]);
    (app.rate_count, app.rate) = (4, 48_000);
    // The Recorder page's keys never touch it.
    for action in [Action::Left, Action::Right, Action::Enter, Action::Resolution, Action::NextDisk] {
        app.act(&mut device, action);
    }
    assert_eq!(app.rate, 48_000);
    app.act(&mut device, Action::View(View::Device));
    for _ in 0..app.device_rows() {
        app.act(&mut device, Action::Down);
    }
    assert_eq!(app.ctl_sel, app.rate_row(), "the last row");
    app.act(&mut device, Action::Right);
    assert_eq!((app.rate, app.message.as_str()), (96_000, "capture at 96000 Hz from now on"));
    app.act(&mut device, Action::Left);
    app.act(&mut device, Action::Left);
    assert_eq!(app.rate, 44_100);
    app.act(&mut device, Action::Left);
    assert_eq!(app.rate, 192_000, "it wraps");
    app.recording.take.running = true;
    app.act(&mut device, Action::Enter);
    assert_eq!((app.rate, app.message.as_str()), (192_000, "stop the take to change the sample rate"));
    app.recording.take.running = false;
    app.recording.request = Some(recorder::Request::Start);
    app.act(&mut device, Action::Enter);
    assert_eq!(app.rate, 192_000, "nor while a take is asked for");
}

#[test]
fn fewer_channels_at_a_higher_rate_keep_the_arming_for_later() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    app.set_capture(30, 48_000, 4, 24, true);
    app.recording.armed = 0b11 << 20 | 1;
    app.recording.input = 25;
    app.metering.input = 29;
    app.analysis.pair = 14;
    // The UFX at 192 kHz: 18 channels.
    app.set_capture(18, 192_000, 4, 24, true);
    let r = &app.recording;
    assert_eq!((r.channels, r.rate, r.armed_count(), r.input), (18, 192_000, 1, 17));
    assert_eq!(r.groups(), ([1, 0, 0, 0], 1), "only input 1 is recorded");
    assert!(app.metering.input < 18 && app.analysis.pair == 8);
    // Only inputs past 18 armed: everything is armed rather than nothing.
    app.recording.armed = 0b11 << 20;
    app.set_capture(18, 192_000, 4, 24, true);
    assert_eq!(app.recording.armed_count(), 18);
    // Back at 48 kHz the earlier arming is there again.
    app.recording.armed = 0b11 << 20 | 1;
    app.set_capture(30, 48_000, 4, 24, true);
    assert_eq!(app.recording.armed_count(), 3);
}

#[test]
fn recorder_time_left_follows_the_largest_run() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    app.recording.choose_disk(None);
    // Two inputs of 24-bit FLAC at 48 kHz, at most 288000 bytes a second; 10 GiB of room.
    assert_eq!(app.recording.seconds_left(), Some((10u64 << 30) / 288_000));
    app.recording.disks[2].largest = u64::MAX;
    assert_eq!(app.recording.seconds_left(), None, "FAT32 does not say");
    assert_eq!(recorder::clock(48_000 * 3725, 48_000).as_str(), "1:02:05");
    assert_eq!(recorder::clock(48_000 * 65, 48_000).as_str(), "01:05");
    assert_eq!(recorder::duration(3 * 3600 + 20 * 60).as_str(), "3 h 20 min");
    assert_eq!(recorder::size(3 << 29).as_str(), "1.5 GiB");
    assert_eq!(recorder::size(640 << 10).as_str(), "640 KiB");
}

#[test]
fn recorder_settings_round_trip() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    let r = &mut app.recording;
    let disk = r.load(|key| match key {
        "rec-ch" => Some("1,3-4"),
        "rec-scale" => Some("nordic"),
        "rec-project" => Some("Gig-at-Ritz"),
        "rec-disk" => Some("ext2"),
        _ => None,
    });
    r.choose_disk(disk);
    assert_eq!((r.armed, r.nordic, r.project.as_str(), r.root(), r.target().unwrap().number), (0b1101, true, "Gig-at-Ritz", "recordings", 2));
    let mut saved = Text::<160>::new();
    r.save(|key, value| {
        let _ = write!(saved, "{key}={value} ");
    });
    assert_eq!(saved.as_str(), "rec-ch=1,3-4 rec-disk=ext2 rec-scale=nordic rec-project=Gig-at-Ritz rec-root=recordings rec-format=flac rec-bits=24 ");
    r.load(|key| (key == "rec-root").then_some("fos/takes"));
    assert_eq!(r.root(), "fos/takes");
    r.load(|key| match key {
        "rec-format" => Some("flac"),
        "rec-bits" => Some("16"),
        _ => None,
    });
    assert_eq!(r.encoding, recorder::Encoding::Flac(16));
    r.load(|key| (key == "rec-format").then_some("wav"));
    assert_eq!(r.encoding, recorder::Encoding::Wav);
    r.load(|key| (key == "rec-bits").then_some("12"));
    assert_eq!(r.encoding, recorder::Encoding::Wav, "a bad depth changes nothing");
    // Bad values leave things as they were.
    r.load(|key| match key {
        "rec-ch" => Some("5"),
        "rec-project" => Some("Gig at Ritz"),
        "rec-root" => Some("/abs"),
        _ => None,
    });
    assert_eq!((r.armed, r.project.as_str(), r.root()), (0b1101, "Gig-at-Ritz", "recordings"), "a bad root is the default");
    // No project yet: none is saved, and the next take makes one up.
    r.project.clear();
    let mut saved = Text::<160>::new();
    r.save(|key, _| {
        let _ = write!(saved, "{key} ");
    });
    assert!(!saved.as_str().contains("rec-project"));
    assert_eq!(r.refusal(), None);
}

#[test]
fn recorder_taps_arm_and_swipes_scroll_the_meters() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    let layout = Layout::new(1280, 720);
    let arm = layout.rec_arm(3, &app).expect("four inputs fit");
    app.press(&mut device, &layout, arm.x + 2, arm.y + 2);
    assert!(app.recording.is_armed(3));
    assert_eq!(app.recording.input, 3);
    let bar = layout.rec_bar(1, &app).unwrap();
    assert_eq!(layout.hit(&app, bar.x + 2, bar.y + 2), Hit::RecMeter(1));
    // Thirty inputs do not fit: a swipe to the left shows later ones.
    app.recording.channels = 30;
    app.scroll(&layout);
    let visible = layout.visible_rec(&app);
    assert!(visible < 30);
    let area = layout.rec_area();
    let grab = app.press(&mut device, &layout, area.x + area.w - 4, area.y + 4).unwrap();
    let grab = app.drag(&mut device, &layout, grab, area.x + area.w - 4 - layout.rec_column_w(&app) * 5, area.y + 4);
    app.release(&mut device, &layout, grab);
    assert_eq!(app.first_rec, 5);
    assert!(app.recording.input >= 5);
}

/// Rack's boot disk: FOSFS (the boot volume) and ext4 over the rest, as `install ... ext4` makes it. The ext volume is a
/// target of its own, named and reached as `ext1`.
#[test]
fn recorder_ext_volumes_are_targets_of_their_own() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    let r = &mut app.recording;
    r.disks[1] = recorder::Disk { volume: recorder::Volume::Ext, writable: true, ..disk(1, recorder::Volume::Fosfs, true, 200 << 30) };
    r.disks[2] = disk(3, recorder::Volume::Fat32, false, 1 << 30);
    r.choose_disk(None);
    assert_eq!(r.target().unwrap().name().as_str(), "ext1", "a writable ext volume, not the same disk's FOSFS");
    assert_eq!(r.target().unwrap().prefix().as_str(), "ext1/");
    r.choose_disk(Some("disk1"));
    assert_eq!(r.target().unwrap().name().as_str(), "ext1", "the same disk's FOSFS volume is not a target");
    let mut saved = Text::<96>::new();
    r.save(|key, value| {
        if key == "rec-disk" {
            let _ = write!(saved, "{value}");
        }
    });
    assert_eq!(saved.as_str(), "ext1");
    // An ext volume the writer cannot keep up with is listed, never chosen, and refused when asked for.
    r.disks[1].writable = false;
    r.choose_disk(None);
    assert_eq!(r.target().unwrap().name().as_str(), "disk1", "nothing to record to: the boot disk, refused");
    r.choose_disk(Some("ext1"));
    assert_eq!(r.refusal(), Some("fos cannot write that ext volume (its features)"));
}

#[test]
fn takes_are_named_with_one_timestamp_a_file() {
    use naming::*;
    let now = Stamp { year: 2026, month: 11, day: 20, hour: 14, minute: 35, second: 12 };
    assert_eq!(take_folder(7, &now).as_str(), "take007_2026-11-20_143512");
    assert_eq!(file_name("Konsert", 7, &now, 0xff, "flac").as_str(), "Konsert_take007_2026-11-20_143512_in01-08.flac");
    assert_eq!(file_name("Konsert", 7, &now, 0x3f00_0000, "flac").as_str(), "Konsert_take007_2026-11-20_143512_in25-30.flac");
    assert_eq!(file_name("Konsert", 12, &now, 0b10, "wav").as_str(), "Konsert_take012_2026-11-20_143512_in02.wav");
    // A made-up project: two words and the minute; its files carry the take's timestamp only.
    let made = made_up_project(0x0102, &now);
    assert_eq!(made.as_str(), "calm-finch_2026-11-20_1435");
    assert!(valid_project_folder(made.as_str()));
    assert_eq!(stem(made.as_str()), "calm-finch");
    assert_eq!(file_name(made.as_str(), 1, &now, 0b11, "flac").as_str(), "calm-finch_take001_2026-11-20_143512_in01-02.flac");
    assert_eq!(stem("Konsert"), "Konsert");
    assert_eq!(stem("Live_2026-11-20_143512"), "Live", "a typed name ending in a take-length stamp too");
    assert_eq!(stem("_2026-11-20_1435"), "_2026-11-20_1435", "nothing is left: kept");
    // Every file name fits exFAT's 60 characters, the longest project and take number included.
    let longest = file_name("ABCDEFGHIJKLMNOPQRST", MAX_TAKE, &now, 0xff00_0000, "flac");
    assert!(longest.as_str().len() <= 60, "{} is {}", longest.as_str(), longest.as_str().len());
    let path = join(&["disk99/", "abcdefghij/abcdefghij/abcdefghij/abcdefg", "ABCDEFGHIJKLMNOPQRST_2026-11-20_1435", take_folder(MAX_TAKE, &now).as_str(), longest.as_str()]);
    assert!(path.as_str().len() < MAX_PATH && path.as_str().ends_with(".flac"), "{}", path.as_str());
    assert_eq!(join(&["ext1/", "recordings", "Konsert"]).as_str(), "ext1/recordings/Konsert");
}

#[test]
fn project_and_root_names_are_plain_ascii() {
    use naming::*;
    for good in ["Konsert", "gig-2", "A_b", "x", "ABCDEFGHIJKLMNOPQRST"] {
        assert!(valid_project(good), "{good}");
    }
    for bad in ["", "-x", "_x", "with space", "räksmörgås", "a.b", "a/b", "ABCDEFGHIJKLMNOPQRSTU", ".."] {
        assert!(!valid_project(bad), "{bad}");
    }
    assert!(valid_project_folder("amber-otter_2026-11-20_1435"));
    assert!(!valid_project_folder("ABCDEFGHIJKLMNOPQRSTU_2026-11-20_1435"), "the stem is still at most 20");
    for good in ["recordings", "fos/recordings", "A-1/b_2"] {
        assert!(valid_root(good), "{good}");
    }
    for bad in ["", "/recordings", "recordings/", "a//b", "a b", "../x", "abcdefghij/abcdefghij/abcdefghij/abcdefghij/"] {
        assert!(!valid_root(bad), "{bad}");
    }
    assert!(same_project("Konsert", "konsert") && !same_project("Konsert", "Konsert2"));
    assert_eq!((take_number("take007_2026-11-20_143512"), take_number("take1234_x"), take_number("take12_x"), take_number("take12345_x"), take_number("takeout"), take_number("Konsert")), (Some(7), Some(1234), None, None, None, None));
    let unset = Stamp { year: 2000, month: 1, day: 1, ..Stamp::default() };
    assert!(!unset.plausible());
    assert!(Stamp { year: 2026, month: 11, day: 20, hour: 23, minute: 59, second: 59 }.plausible());
}

#[test]
fn the_project_is_typed_with_every_key_going_to_the_field() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    app.recording.project = naming::ProjectName::from("keen-gull_2026-11-20_2216");
    app.act(&mut device, Action::Pair);
    assert_eq!(app.editing, Some(Text::from("keen-gull")), "the field starts with the stem");
    assert_eq!((app.soft_keys()[SOFT_NAV].label, app.soft_keys()[SOFT_NAV + 1].label), ("Keep", "Cancel"));
    // Enter with the stem unchanged keeps the made-up project, timestamp and all.
    app.act(&mut device, Action::Enter);
    assert_eq!((app.editing.is_none(), app.recording.project.as_str()), (true, "keen-gull_2026-11-20_2216"));
    app.act(&mut device, Action::Pair);
    for _ in 0..9 {
        app.type_project(Edit::Erase);
    }
    // Shortcut letters and space are text or refused here, never a take.
    for c in b"Ra dio" {
        app.type_project(Edit::Char(*c));
    }
    assert_eq!((app.editing.unwrap().as_str(), app.recording.request), ("Radio", None));
    app.type_project(Edit::Char(b'.'));
    assert_eq!(app.message.as_str(), "only A-Z a-z 0-9 - _ (names travel to other systems)");
    for c in b"-Konsert-2026-ABCDEF" {
        app.type_project(Edit::Char(*c));
    }
    assert_eq!(app.editing.unwrap().as_str().len(), naming::MAX_PROJECT, "it stops at 20");
    app.type_project(Edit::Cancel);
    assert_eq!(app.recording.project.as_str(), "keen-gull_2026-11-20_2216", "Tab cancels");
    app.act(&mut device, Action::Pair);
    for _ in 0..9 {
        app.type_project(Edit::Erase);
    }
    for c in b"-x" {
        app.type_project(Edit::Char(*c));
    }
    app.type_project(Edit::Keep);
    assert_eq!(app.message.as_str(), "a project name starts with a letter or a digit");
    assert!(app.editing.is_some(), "still typing");
    app.type_project(Edit::Erase);
    app.type_project(Edit::Erase);
    for c in b"Konsert" {
        app.type_project(Edit::Char(*c));
    }
    app.act(&mut device, Action::Enter);
    assert_eq!(app.recording.project.as_str(), "Konsert");
    assert_eq!(app.recording.persisted().3.as_str(), "Konsert", "saved with the rest");
    // An empty name: none, made up at the next take.
    app.act(&mut device, Action::Pair);
    for _ in 0..7 {
        app.type_project(Edit::Erase);
    }
    app.type_project(Edit::Keep);
    assert!(app.recording.project.is_empty());
    // A page switch ends typing; during a take the project is locked.
    app.act(&mut device, Action::Pair);
    app.act(&mut device, Action::Page(Page::Meters));
    assert!(app.editing.is_none() && app.page == Page::Meters);
    app.act(&mut device, Action::Page(Page::Recorder));
    app.recording.take.running = true;
    app.act(&mut device, Action::Pair);
    assert_eq!((app.editing.is_none(), app.message.as_str()), (true, "stop the take to change the project"));
}

#[test]
fn recorder_panel_rows_are_buttons() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    let layout = Layout::new(1280, 720);
    app.recording.choose_disk(None);
    let tap = |app: &mut App, device: &mut Fake, row: RecRow| {
        let area = layout.rec_row(row);
        assert_eq!(layout.hit(app, area.x + 4, area.y + area.h / 2), if row.tappable() { Hit::RecRow(row) } else { Hit::Nothing });
        app.press(device, &layout, area.x + 4, area.y + area.h / 2);
    };
    tap(&mut app, &mut device, RecRow::Format);
    assert_eq!(app.recording.encoding, recorder::Encoding::Flac(20));
    tap(&mut app, &mut device, RecRow::Disk);
    assert_eq!(app.recording.target().unwrap().name().as_str(), "disk1", "the next disk (wraps)");
    app.recording.armed = 1;
    tap(&mut app, &mut device, RecRow::Inputs);
    assert_eq!(app.recording.armed_count(), 4);
    tap(&mut app, &mut device, RecRow::Room);
    tap(&mut app, &mut device, RecRow::Project);
    assert!(app.editing.is_some());
    // While typing, the panel and the arm buttons do nothing; the soft keys still answer.
    tap(&mut app, &mut device, RecRow::Format);
    assert_eq!(app.recording.encoding, recorder::Encoding::Flac(20));
    let cancel = layout.soft(&app, SOFT_NAV + 1);
    app.press(&mut device, &layout, cancel.x + 2, cancel.y + 2);
    assert!(app.editing.is_none());
    // Rows sit under the REC lamp, inside the panel, one after another, a finger high.
    let (first, last) = (layout.rec_row(RecRow::File), layout.rec_row(RecRow::Warning));
    assert!(first.y > layout.rec_panel().y + layout.rec_big() && last.y + last.h <= layout.rec_panel().y + layout.rec_panel().h);
    assert!(first.h >= layout.row * 2 - 4);
}

#[test]
fn record_and_stop_are_two_buttons_and_arm_buttons_take_a_finger() {
    let mut device = Fake::new();
    let mut app = recorder_app(&mut device);
    app.recording.choose_disk(None);
    let layout = Layout::new(1280, 800);
    let (record, stop) = (layout.rec_transport(Transport::Record), layout.rec_transport(Transport::Stop));
    assert_eq!(layout.hit(&app, record.x + record.w / 2, record.y + record.h / 2), Hit::Transport(Transport::Record));
    assert_eq!(layout.hit(&app, stop.x + stop.w / 2, stop.y + stop.h / 2), Hit::Transport(Transport::Stop));
    assert!(record.x + record.w < stop.x && record.y == stop.y && record.h >= 44, "side by side, a fingertip high");
    assert!(record.y > layout.rec_panel().y + layout.rec_big(), "under the take clock");
    // Stop with no take does nothing; Record asks for one.
    app.press(&mut device, &layout, stop.x + 10, stop.y + 10);
    assert_eq!(app.recording.request, None);
    app.press(&mut device, &layout, record.x + 10, record.y + 10);
    assert_eq!(app.recording.request, Some(recorder::Request::Start));
    app.recording.request = None;
    app.recording.take.running = true;
    // A second press of Record (a bounce) leaves the take running; only Stop stops it.
    app.press(&mut device, &layout, record.x + 10, record.y + 10);
    assert_eq!(app.recording.request, None);
    app.press(&mut device, &layout, stop.x + 10, stop.y + 10);
    assert_eq!(app.recording.request, Some(recorder::Request::Stop));
    // The soft keys are Record and Stop too; space still toggles.
    let labels = app.soft_keys().map(|key| key.label);
    assert!(labels.contains(&"Record") && labels.contains(&"Stop"), "{labels:?}");
    assert!(layout.rec_row(RecRow::File).y >= record.y + record.h, "the rows start under them");
    let last = layout.rec_row(RecRow::Warning);
    assert!(last.y + last.h <= layout.rec_panel().y + layout.rec_panel().h, "the last row fits");
    // The UC's 18 inputs: each arm button at least 48 x 44 pixels; the rest of the inputs are swiped to.
    app.recording.channels = 18;
    let arm = layout.rec_arm(0, &app).unwrap();
    assert!(arm.w >= 42 && arm.h >= 44, "{} x {}", arm.w, arm.h);
    assert!(layout.visible_rec(&app) >= 8 && layout.visible_rec(&app) < 18);
}
