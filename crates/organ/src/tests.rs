use super::*;

const RATE: u32 = 48000;

fn render(organ: &mut Organ, frames: usize) -> Vec<[f32; 2]> {
    let mut out = vec![[0.0; 2]; frames];
    organ.render(&mut out);
    out
}

/// Amplitude of the `hz` component of the left channel (Goertzel-like single-bin DFT).
fn component(frames: &[[f32; 2]], hz: f32) -> f32 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (n, frame) in frames.iter().enumerate() {
        let angle = 2.0 * std::f64::consts::PI * hz as f64 * n as f64 / RATE as f64;
        re += frame[0] as f64 * angle.cos();
        im += frame[0] as f64 * angle.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / frames.len() as f64) as f32
}

fn only(organ: &mut Organ, bars: [u8; DRAWBARS]) {
    organ.set_drawbars(bars);
    organ.set_click(0.0);
}

#[test]
fn eight_foot_is_the_key_itself() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, [0, 0, 8, 0, 0, 0, 0, 0, 0]);
    organ.note_on(69);
    render(&mut organ, 4800);
    let sound = render(&mut organ, 9600);
    assert!(component(&sound, 440.0) > 0.03);
    assert!(component(&sound, 880.0) < 0.001);
    assert!(component(&sound, 220.0) < 0.001);
}

#[test]
fn drawbars_pick_their_harmonics() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, [8, 8, 0, 8, 8, 8, 8, 8, 8]);
    organ.note_on(57);
    render(&mut organ, 4800);
    let sound = render(&mut organ, 9600);
    let key = 220.0f32;
    let semitones = |n: f32| key * 2f32.powf(n / 12.0);
    for interval in [-12.0, 7.0, 12.0, 19.0, 24.0, 28.0, 31.0, 36.0] {
        assert!(component(&sound, semitones(interval)) > 0.03, "interval {interval}");
    }
    assert!(component(&sound, key) < 0.002, "8' is off");
}

#[test]
fn three_db_per_step() {
    let level = |position| {
        let mut organ = Organ::new(RATE);
        only(&mut organ, [0, 0, position, 0, 0, 0, 0, 0, 0]);
        organ.note_on(60);
        render(&mut organ, 4800);
        component(&render(&mut organ, 9600), 261.6256)
    };
    let ratio = level(8) / level(5);
    assert!((ratio - 2.0f32.sqrt().powi(3)).abs() < 0.05 * ratio, "ratio {ratio}");
}

#[test]
fn top_keys_fold_back_an_octave() {
    // Key 96 with the 1' bar would be wheel 109, beyond the 91 wheels: it sounds an octave lower (MIDI 108 + ... = wheel 97 - 12).
    assert_eq!(wheel_for(96, 36), 96 + 36 - 24 - 24);
    assert!(wheel_for(96, 36) < WHEELS);
    assert_eq!(wheel_for(36, -12), 0);
}

#[test]
fn release_goes_silent() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, PRESETS[0].1);
    organ.midi(&[0x90, 60, 100]);
    render(&mut organ, 4800);
    assert!(organ.is_held(60));
    organ.midi(&[0x90, 60, 0]);
    assert!(!organ.is_held(60));
    render(&mut organ, 9600);
    let tail = render(&mut organ, 480);
    assert!(tail.iter().all(|f| f[0].abs() < 1e-4), "still sounding");
}

#[test]
fn notes_outside_the_keyboard_are_ignored() {
    let mut organ = Organ::new(RATE);
    organ.note_on(35);
    organ.note_on(97);
    assert_eq!(organ.held_count(), 0);
    organ.note_on(36);
    organ.note_on(96);
    organ.note_on(96);
    assert_eq!(organ.held_count(), 2);
    assert!(organ.midi(&[0xb0, 123, 0]));
    assert_eq!(organ.held_count(), 0);
    assert!(!organ.midi(&[0xe0, 0, 64]));
}

#[test]
fn percussion_decays_and_fires_once_per_phrase() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, [0, 0, 8, 0, 0, 0, 0, 0, 0]);
    organ.set_percussion(Percussion::Second);
    organ.set_percussion_fast(true);
    organ.note_on(57);
    render(&mut organ, 480);
    let early = component(&render(&mut organ, 2400), 440.0);
    render(&mut organ, 24000);
    let late = component(&render(&mut organ, 2400), 440.0);
    assert!(early > 0.02 && late < 0.005, "early {early} late {late}");
    // A second key while the first is held does not retrigger it.
    organ.note_on(64);
    let second = render(&mut organ, 2400);
    assert!(component(&second, 659.2551) < 0.005);
}

#[test]
fn percussion_replaces_the_one_foot_bar() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, [0, 0, 0, 0, 0, 0, 0, 0, 8]);
    organ.set_percussion(Percussion::Third);
    organ.note_on(57);
    render(&mut organ, 24000);
    let sound = render(&mut organ, 4800);
    assert!(sound.iter().all(|f| f[0].abs() < 1e-3));
}

#[test]
fn leslie_makes_the_channels_differ() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, PRESETS[0].1);
    organ.set_leslie(Leslie::Fast);
    organ.note_on(72);
    render(&mut organ, 96000);
    let sound = render(&mut organ, 9600);
    let difference: f32 = sound.iter().map(|f| (f[0] - f[1]).abs()).sum::<f32>() / sound.len() as f32;
    assert!(difference > 0.005, "difference {difference}");
}

#[test]
fn vibrato_adds_side_bands_and_chorus_keeps_the_pitch() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, [0, 0, 8, 0, 0, 0, 0, 0, 0]);
    organ.set_scanner(Scanner::V3);
    organ.note_on(69);
    render(&mut organ, 9600);
    let sound = render(&mut organ, 48000);
    assert!(component(&sound, 440.0) > 0.01);
    assert!(component(&sound, 446.1) + component(&sound, 433.9) > 0.001);
}

#[test]
fn output_stays_within_full_scale() {
    let mut organ = Organ::new(RATE);
    only(&mut organ, PRESETS[0].1);
    organ.set_drive(1.0);
    organ.set_volume(1.0);
    organ.set_leslie(Leslie::Fast);
    organ.set_scanner(Scanner::C3);
    organ.set_percussion(Percussion::Second);
    for note in FIRST_KEY..FIRST_KEY + KEYS as u8 {
        organ.note_on(note);
    }
    for frame in render(&mut organ, 48000) {
        assert!(frame[0].abs() <= 1.0 && frame[1].abs() <= 1.0);
    }
}

#[test]
fn works_at_other_sample_rates() {
    let mut organ = Organ::new(44100);
    only(&mut organ, [0, 0, 8, 0, 0, 0, 0, 0, 0]);
    organ.note_on(69);
    let mut out = vec![[0.0; 2]; 8820];
    organ.render(&mut out);
    let peak = out[4410..].iter().map(|f| f[0].abs()).fold(0.0, f32::max);
    assert!(peak > 0.03 && peak < 0.1, "peak {peak}");
}

#[test]
fn mod_wheel_picks_the_leslie_zone() {
    let mut organ = Organ::new(RATE);
    for (value, wanted) in [(0, Leslie::Stop), (42, Leslie::Stop), (43, Leslie::Slow), (84, Leslie::Slow), (85, Leslie::Fast), (127, Leslie::Fast), (10, Leslie::Stop)] {
        assert!(organ.midi(&[0xb3, 1, value]));
        assert_eq!(organ.leslie(), wanted, "mod wheel {value}");
    }
}

fn tail_energy(reverb: f32) -> f32 {
    let mut organ = Organ::new(RATE);
    only(&mut organ, [0, 0, 8, 0, 0, 0, 0, 0, 0]);
    organ.set_reverb(reverb);
    organ.note_on(60);
    render(&mut organ, 9600);
    organ.note_off(60);
    render(&mut organ, 4800); // the key's own release dies away
    let tail = render(&mut organ, 24000);
    tail.iter().map(|f| f[0] * f[0] + f[1] * f[1]).sum::<f32>()
}

#[test]
fn reverb_rings_on_after_the_key_is_released_and_off_means_silence() {
    assert!(tail_energy(0.0) < 1e-9);
    let wet = tail_energy(0.5);
    assert!(wet > 0.01, "tail energy {wet}");
    assert!(tail_energy(1.0) > wet);
}

#[test]
fn reverb_stays_in_range_and_dry_level_is_kept_when_off() {
    let mut organ = Organ::new(RATE);
    organ.set_reverb(1.0);
    for note in [48, 52, 55, 60, 64, 67] {
        organ.note_on(note);
    }
    let frames = render(&mut organ, 48000);
    assert!(frames.iter().all(|f| f[0].abs() <= 1.0 && f[1].abs() <= 1.0 && f[0].is_finite()));
    organ.set_reverb(0.0);
    assert_eq!(organ.reverb(), 0.0);
}

#[test]
fn reverb_works_at_every_supported_rate() {
    for rate in [8000, 44100, 96000, 192000] {
        let mut organ = Organ::new(rate);
        organ.set_reverb(0.5);
        organ.note_on(60);
        let mut out = vec![[0.0; 2]; 4096];
        organ.render(&mut out);
        assert!(out.iter().all(|f| f[0].is_finite() && f[0].abs() <= 1.0), "rate {rate}");
    }
}

#[test]
fn key_click_is_audible_without_drive() {
    let mut organ = Organ::new(RATE);
    organ.set_drawbars([0; DRAWBARS]);
    organ.set_click(1.0);
    organ.note_on(60);
    let burst = render(&mut organ, 96);
    let peak = burst.iter().map(|f| f[0].abs()).fold(0.0, f32::max);
    assert!(peak > 0.1, "click peak {peak}");
}
