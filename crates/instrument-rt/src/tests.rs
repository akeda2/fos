use super::*;
use audio_sample::Format;
use tape_rt::TouchEvent;

fn event(id: i32, x: i32, y: i32, down: bool) -> TouchEvent {
    TouchEvent { panel: 0, id, x, y, down, screen: -1 }
}

#[derive(Default)]
struct Recorder {
    log: std::vec::Vec<std::string::String>,
}

impl TouchTarget for Recorder {
    type Grab = i32;
    fn down(&mut self, x: i32, y: i32) -> Option<i32> {
        self.log.push(format!("down {x},{y}"));
        (x < 100).then_some(x)
    }
    fn moved(&mut self, grab: i32, x: i32, y: i32) -> i32 {
        self.log.push(format!("moved {grab} {x},{y}"));
        x
    }
    fn up(&mut self, grab: i32) {
        self.log.push(format!("up {grab}"));
    }
}

#[test]
fn touches_scale_panel_units_and_follow_the_grab() {
    let mut touches = Touches::new(1000, 500, 0);
    let mut target = Recorder::default();
    // A panel of 4000 x 2000 units: (200, 100) is (50, 25) on the picture.
    assert!(touches.handle(&event(1, 200, 100, true), (4000, 2000), &mut target));
    assert!(touches.handle(&event(1, 240, 120, true), (4000, 2000), &mut target));
    assert!(touches.handle(&event(1, 240, 120, false), (4000, 2000), &mut target));
    assert_eq!(target.log, ["down 50,25", "moved 50 60,30", "up 60"]);
}

#[test]
fn touches_ignore_fingers_that_grabbed_nothing_and_other_screens() {
    let mut touches = Touches::new(1000, 500, 1);
    let mut target = Recorder::default();
    touches.handle(&event(1, 3000, 0, true), (4000, 2000), &mut target); // x = 750: nothing there
    touches.handle(&event(1, 3000, 0, true), (4000, 2000), &mut target); // still nothing held: a fresh down
    touches.handle(&event(1, 3000, 0, false), (4000, 2000), &mut target);
    assert_eq!(target.log, ["down 750,0", "down 750,0"]);
    let other = TouchEvent { screen: 0, ..event(2, 0, 0, true) };
    assert!(!touches.handle(&other, (4000, 2000), &mut target));
    assert_eq!(target.log.len(), 2);
}

#[test]
fn touches_hold_eight_fingers_at_once() {
    let mut touches = Touches::new(100, 100, 0);
    let mut target = Recorder::default();
    for id in 0..9 {
        touches.handle(&event(id, 1, 1, true), (100, 100), &mut target);
    }
    // The ninth finger is a down that found no free slot and so is never offered to the target.
    assert_eq!(target.log.len(), 8);
    touches.handle(&event(0, 1, 1, false), (100, 100), &mut target);
    touches.handle(&event(8, 1, 1, true), (100, 100), &mut target);
    assert_eq!(target.log.len(), 10);
}

#[test]
fn typed_keys_ring_for_a_while_and_repeat_extends() {
    let mut keys = TypedKeys::new(300);
    keys.press(60, 0);
    keys.press(64, 100);
    keys.press(60, 200);
    let mut ended = std::vec::Vec::new();
    assert!(!keys.expire(299, |n| ended.push(n)));
    assert!(keys.expire(400, |n| ended.push(n)));
    assert_eq!(ended, [64]);
    assert!(keys.expire(500, |n| ended.push(n)));
    assert_eq!(ended, [64, 60]);
    assert!(!keys.expire(1000, |n| ended.push(n)));
}

#[test]
fn pack_puts_the_pair_on_the_chosen_channels() {
    let format = Format::new(4, 32, true);
    let frames = [[0.5, -0.25]];
    let mut out = [0u8; 4 * 4 * 2];
    pack(&frames, 4, 2, format, &mut out);
    let sample = |channel: usize| format.read(&out[channel * 4..channel * 4 + 4]);
    assert_eq!((sample(0), sample(1), sample(2), sample(3)), (0.0, 0.0, 0.5, -0.25));
}

#[test]
fn pack_mixes_down_for_a_mono_device_and_writes_integers() {
    let float = Format::new(4, 32, true);
    let mut out = [0u8; 4];
    pack(&[[0.5, 0.25]], 1, 0, float, &mut out);
    assert_eq!(float.read(&out), 0.375);
    let int = Format::new(3, 24, false);
    let mut out = [0u8; 6];
    pack(&[[0.5, -0.5]], 2, 0, int, &mut out);
    assert!((int.read(&out[..3]) - 0.5).abs() < 1e-6 && (int.read(&out[3..]) + 0.5).abs() < 1e-6);
}

#[test]
fn every_key_is_found_at_its_own_centre_and_typed_keys_map() {
    for (w, h) in [(640, 480), (1280, 1024), (1920, 1080)] {
        let keyboard = Keyboard::new(w, h * 7 / 10, h * 3 / 10 - 8, 8);
        for note in FIRST_KEY..FIRST_KEY + KEYS as u8 {
            let rect = keyboard.key_rect(note);
            let black = matches!((note - FIRST_KEY) % 12, 1 | 3 | 6 | 8 | 10);
            let y = if black { rect.y + rect.h / 2 } else { rect.y + rect.h - 3 };
            assert_eq!(keyboard.hit(rect.x + rect.w / 2, y), Some(note), "{w}x{h} note {note}");
        }
        assert_eq!(keyboard.hit(0, 0), None);
    }
    assert_eq!(note_for_char(b'z', 48), Some(48));
    assert_eq!(note_for_char(b'Q', 48), Some(60));
    assert_eq!(note_for_char(b'z', 97), None);
    assert_eq!(note_for_char(b'!', 48), None);
}

#[test]
fn pack_wide_puts_the_channels_from_first_and_leaves_silence_around() {
    let format = Format::new(4, 32, true);
    let frames = [[0.1, 0.2, 0.0, 0.3, 0.4, 0.5]];
    let mut out = [0u8; 8 * 4];
    pack_wide(&frames, 4, 8, 1, format, &mut out);
    let sample = |channel: usize| format.read(&out[channel * 4..channel * 4 + 4]);
    assert_eq!([0, 1, 2, 3, 4, 5, 6, 7].map(sample), [0.0, 0.1, 0.2, 0.0, 0.3, 0.0, 0.0, 0.0]);
}
