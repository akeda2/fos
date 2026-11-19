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
}

impl Fake {
    pub fn new() -> Fake {
        Fake { gain: [[None, None, Some(UNITY), Some(0)], [None, None, Some(0), Some(UNITY)]], master: [UNITY; 2], controls: [0; 3], serial: 1, writes: 0, refuse: false, unknown_trim: false }
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
            0 => "Dim",
            1 => {
                control.flags = MIXER_FLAG_HAZARD;
                "Phantom"
            }
            2 => {
                control.kind = MIXER_CONTROL_INT;
                control.min = -10;
                control.max = 10;
                if self.unknown_trim {
                    control.flags = fosfor_abi::tape::MIXER_FLAG_UNKNOWN;
                }
                "Trim"
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
    for _ in 0..3 {
        app.act(&mut device, Action::Down);
    }
    assert_eq!(app.ctl_sel, app.ask_row());
    app.act(&mut device, Action::Enter);
    assert!(app.ask_hazard && app.ask_changed, "the last row switches asking on");
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
fn the_mouse_drags_faders_and_pan_and_presses_soft_keys() {
    let mut device = Fake::new();
    let mut app = app(&mut device);
    let layout = Layout::new(1280, 720);
    app.scroll(&layout);
    let fader = layout.fader(1, &app).unwrap();
    let grab = app.press(&mut device, &layout, fader.x + 2, fader.y);
    assert_eq!(grab, Some(Grab::Fader(1)));
    assert_eq!(device.gain[0][2], Some(65_381), "the top is +6 dB");
    app.drag(&mut device, &layout, Grab::Fader(1), fader.x + 2, fader.y + fader.h);
    assert_eq!(device.gain[0][2], Some(0), "the bottom is off");
    app.drag(&mut device, &layout, Grab::Fader(1), fader.x + 2, fader.y + fader.h - (fader.h as f64 * layout::fader_position(0.0)) as i32);
    assert_eq!(app.strip_level(1).0, 0.0);
    let pan = layout.pan(1, &app).unwrap();
    assert_eq!(app.press(&mut device, &layout, pan.x, pan.y + 1), Some(Grab::Pan(1)));
    assert_eq!(device.gain[1][3], Some(0), "hard left on a pair turns the right side off");
    let matrix = layout.soft(2);
    assert_eq!(app.press(&mut device, &layout, matrix.x + 1, matrix.y + 1), None);
    assert_eq!(app.view, View::Matrix);
    app.scroll(&layout);
    let cell = layout.cell(1, 0, &app).unwrap();
    app.press(&mut device, &layout, cell.x + 1, cell.y + 1);
    assert_eq!((app.row, app.col, app.view), (1, 0, View::Matrix));
    app.press(&mut device, &layout, cell.x + 1, cell.y + 1);
    assert_eq!((app.src_sel, app.view), (1, View::Outputs), "a second click opens the send");
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
