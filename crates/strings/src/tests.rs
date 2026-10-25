use super::*;

const RATE: u32 = 48000;

fn render(machine: &mut Strings, frames: usize) -> Vec<[f32; 2]> {
    let mut out = vec![[0.0; 2]; frames];
    machine.render(&mut out);
    out
}

fn rms(frames: &[[f32; 2]]) -> f32 {
    (frames.iter().map(|f| f[0] * f[0]).sum::<f32>() / frames.len() as f32).sqrt()
}

/// Amplitude of the `hz` component of the left channel.
fn component(frames: &[[f32; 2]], hz: f32) -> f32 {
    let (mut re, mut im) = (0.0f64, 0.0f64);
    for (n, frame) in frames.iter().enumerate() {
        let angle = 2.0 * std::f64::consts::PI * hz as f64 * n as f64 / RATE as f64;
        re += frame[0] as f64 * angle.cos();
        im += frame[0] as f64 * angle.sin();
    }
    (2.0 * (re * re + im * im).sqrt() / frames.len() as f64) as f32
}

/// A dry machine with only `section` on and quick envelopes.
fn dry(section: usize) -> Strings {
    let mut machine = Strings::new(RATE);
    for s in 0..SECTIONS {
        machine.set_level(s, if s == section { 1.0 } else { 0.0 });
    }
    machine.set_ensemble(0.0);
    machine.set_phaser(0.0);
    machine.set_reverb(0.0);
    machine.echo_mut().set_echo_level(0.0);
    machine.set_attack(0.0);
    machine.set_release(0.0);
    machine
}

#[test]
fn idle_is_silent() {
    let mut machine = Strings::new(RATE);
    assert!(render(&mut machine, 2000).iter().all(|f| *f == [0.0, 0.0]));
}

#[test]
fn octaves_are_divided_down_from_one_generator() {
    let mut machine = Strings::new(RATE);
    let wraps = |machine: &mut Strings, note: i32| {
        let mut last = machine.saw(note);
        let mut count = 0;
        for _ in 0..RATE {
            for pc in 0..12 {
                machine.counter[pc] = machine.counter[pc].wrapping_add(machine.step[pc] as u64);
            }
            let now = machine.saw(note);
            if now < last - 1.0 {
                count += 1;
            }
            last = now;
        }
        count
    };
    // One second: A3 (57) is 220 Hz, A4 (69) is 440 Hz.
    let (low, high) = (wraps(&mut machine, 57), wraps(&mut machine, 69));
    assert!((low as i32 - 220).abs() <= 1 && (high as i32 - 440).abs() <= 1, "{low} {high}");
}

#[test]
fn a_key_sounds_at_its_pitch_on_the_chosen_section() {
    let mut machine = dry(1); // viola: the 8' register
    machine.note_on(57);
    render(&mut machine, 9600);
    let sound = render(&mut machine, 9600);
    assert!(component(&sound, 220.0) > 0.01, "{}", component(&sound, 220.0));
    assert!(component(&sound, 220.0) > 8.0 * component(&sound, 233.0));
    assert!(rms(&sound) > 0.0);
}

#[test]
fn sections_have_their_registers() {
    // The bass plays the 16', the violin has the 4' on top of the 8'.
    let mut bass = dry(3);
    bass.note_on(57);
    render(&mut bass, 9600);
    let sound = render(&mut bass, 9600);
    assert!(component(&sound, 110.0) > 0.01);
    assert!(component(&sound, 110.0) > component(&sound, 220.0));
    let mut violin = dry(0);
    violin.note_on(57);
    render(&mut violin, 9600);
    let sound = render(&mut violin, 9600);
    assert!(component(&sound, 440.0) > 0.01 && component(&sound, 110.0) < 0.001);
}

#[test]
fn a_section_at_zero_is_silent() {
    let mut machine = dry(0);
    machine.set_level(0, 0.0);
    machine.note_on(60);
    assert!(render(&mut machine, 9600).iter().all(|f| f[0].abs() < 1e-6));
}

#[test]
fn attack_and_release_follow_the_sliders() {
    let mut slow = dry(1);
    slow.set_attack(1.0);
    slow.note_on(60);
    let early = rms(&render(&mut slow, 480));
    render(&mut slow, RATE as usize * 3);
    let late = rms(&render(&mut slow, 4800));
    assert!(early < 0.1 * late, "{early} {late}");

    let mut quick = dry(1);
    quick.note_on(60);
    render(&mut quick, 9600);
    quick.note_off(60);
    render(&mut quick, RATE as usize);
    assert!(rms(&render(&mut quick, 4800)) < 1e-4);

    let mut long = dry(1);
    long.set_release(1.0);
    long.note_on(60);
    render(&mut long, 9600);
    long.note_off(60);
    render(&mut long, 4800);
    assert!(rms(&render(&mut long, 4800)) > 0.002);
}

#[test]
fn the_sustain_pedal_holds_released_keys() {
    let mut machine = dry(1);
    machine.midi(&[0xb0, 64, 127]);
    machine.midi(&[0x90, 60, 100]);
    render(&mut machine, 9600);
    machine.midi(&[0x80, 60, 0]);
    render(&mut machine, RATE as usize);
    assert!(rms(&render(&mut machine, 4800)) > 0.01);
    assert_eq!(machine.held_count(), 0);
    machine.midi(&[0xb0, 64, 0]);
    render(&mut machine, RATE as usize);
    assert!(rms(&render(&mut machine, 4800)) < 1e-4);
}

#[test]
fn the_piano_decays_while_held_and_strings_do_not() {
    let mut piano = dry(5);
    piano.note_on(60);
    let start = rms(&render(&mut piano, 4800));
    render(&mut piano, RATE as usize * 3);
    let end = rms(&render(&mut piano, 4800));
    assert!(end < 0.2 * start, "{start} {end}");
    let mut viola = dry(1);
    viola.note_on(60);
    render(&mut viola, 9600);
    let start = rms(&render(&mut viola, 4800));
    render(&mut viola, RATE as usize * 3);
    assert!(rms(&render(&mut viola, 4800)) > 0.9 * start);
}

#[test]
fn the_mod_wheel_steers_what_the_switch_says() {
    let mut machine = Strings::new(RATE);
    machine.set_mod_target(ModTarget::Phaser);
    machine.midi(&[0xb0, 1, 0]);
    let (slow, depth) = (machine.phaser_speed(), machine.ensemble_depth());
    machine.midi(&[0xb0, 1, 127]);
    assert!(machine.phaser_speed() > 10.0 * slow && machine.ensemble_depth() == depth);
    machine.set_mod_target(ModTarget::Ensemble);
    let speed = machine.phaser_speed();
    machine.midi(&[0xb0, 1, 0]);
    assert!(machine.phaser_speed() == speed && machine.ensemble_depth() < 0.5);
    machine.set_mod_target(ModTarget::Both);
    machine.midi(&[0xb0, 1, 127]);
    assert!(machine.phaser_speed() > 5.0 && machine.ensemble_depth() > 0.95);
    machine.set_mod_target(ModTarget::Wah);
    assert!(machine.phaser_speed() == 0.4 && machine.ensemble_depth() == 0.8);
}

/// How open C3 sounds through the wah: harmonics 6-7 (785-915 Hz, an "a") over 2-3 (260-390 Hz, a "u").
fn openness(frames: &[[f32; 2]]) -> f32 {
    let band = |range: core::ops::RangeInclusive<u32>| range.map(|n| component(frames, 130.81 * n as f32)).sum::<f32>();
    band(6..=7) / band(2..=3)
}

fn wah(env: f32) -> Strings {
    let mut machine = dry(0);
    machine.set_wah(true);
    machine.set_vowel(0.0);
    machine.set_env(env);
    machine.note_on(48);
    render(&mut machine, 4800);
    machine
}

#[test]
fn the_pedal_and_the_wheel_open_the_wah_and_off_is_off() {
    let mut machine = wah(0.0);
    let closed = openness(&render(&mut machine, 9600));
    assert!(machine.midi(&[0xb0, 11, 110]));
    render(&mut machine, 480);
    let pedal = openness(&render(&mut machine, 9600));
    machine.midi(&[0xb0, 11, 0]);
    machine.set_mod_target(ModTarget::Wah);
    machine.set_mod_wheel(110.0 / 127.0);
    render(&mut machine, 480);
    let wheel = openness(&render(&mut machine, 9600));
    machine.set_wah(false);
    render(&mut machine, 480);
    let off = openness(&render(&mut machine, 9600));
    assert!(pedal > 1.8 * closed && (wheel - pedal).abs() < 0.1 * pedal && off < 0.7 * pedal, "{closed} {pedal} {wheel} {off}");
}

#[test]
fn the_envelope_opens_the_wah_as_the_sound_swells() {
    let mut machine = wah(0.0);
    let still = openness(&render(&mut machine, 9600));
    let mut machine = wah(1.0);
    let opened = openness(&render(&mut machine, 9600));
    assert!(opened > 1.5 * still, "{still} {opened}");
}

#[test]
fn a_chord_through_a_sharp_wah_stays_in_range() {
    let mut machine = Strings::new(RATE);
    machine.set_wah(true);
    machine.set_q(1.0);
    machine.set_range(1.0);
    machine.set_volume(1.0);
    machine.midi(&[0xb0, 11, 70]);
    for note in [36, 48, 55, 60, 64, 67, 72, 84] {
        machine.note_on(note);
    }
    let out = render(&mut machine, RATE as usize);
    assert!(out.iter().all(|f| f[0].is_finite() && f[0].abs() < 1.0), "{}", out.iter().fold(0.0f32, |p, f| p.max(f[0].abs())));
    assert!(rms(&out) > 0.01);
}

#[test]
fn unknown_messages_are_ignored_and_all_notes_off_works() {
    let mut machine = Strings::new(RATE);
    assert!(!machine.midi(&[0xe0, 0, 64]) && !machine.midi(&[0xb0, 7, 100]) && !machine.midi(&[0xf8]));
    machine.note_on(48);
    machine.note_on(5); // outside the keyboard
    assert_eq!(machine.held_count(), 1);
    assert!(machine.midi(&[0xb0, 123, 0]));
    assert_eq!(machine.held_count(), 0);
}

#[test]
fn a_full_chord_with_everything_on_stays_in_range_at_every_rate() {
    for rate in [8000, 44100, 48000, 96000, 192000] {
        let mut machine = Strings::new(rate);
        for s in 0..SECTIONS {
            machine.set_level(s, 1.0);
        }
        machine.set_ensemble(1.0);
        machine.set_phaser(1.0);
        machine.set_reverb(1.0);
        machine.set_volume(1.0);
        for note in 36..=96 {
            machine.note_on(note);
        }
        let mut out = vec![[0.0; 2]; rate as usize / 2];
        machine.render(&mut out);
        assert!(out.iter().all(|f| f[0].is_finite() && f[1].is_finite() && f[0].abs() <= 1.0 && f[1].abs() <= 1.0), "{rate}");
        assert!(out.iter().any(|f| f[0].abs() > 0.01), "{rate}");
    }
}

#[test]
fn a_moderate_chord_does_not_clip() {
    let mut machine = Strings::new(RATE);
    for note in [48, 55, 60, 64, 67, 72] {
        machine.note_on(note);
    }
    let out = render(&mut machine, RATE as usize * 2);
    let peak = out.iter().fold(0.0f32, |peak, f| peak.max(f[0].abs()).max(f[1].abs()));
    assert!(peak > 0.05 && peak < 0.95, "{peak}");
}

#[test]
fn the_tape_echo_repeats_a_note_after_it_ends() {
    let tail = |echo_level: f32| {
        let mut machine = dry(1);
        machine.echo_mut().set_echo_level(echo_level);
        machine.echo_mut().set_repeat_rate(1.0);
        machine.note_on(60);
        render(&mut machine, RATE as usize / 2);
        machine.note_off(60);
        // With no release, only the echoes (and their feedback) can still be sounding 0.25 s after the key.
        render(&mut machine, RATE as usize / 4);
        rms(&render(&mut machine, RATE as usize / 4))
    };
    assert!(tail(0.0) < 1e-4);
    assert!(tail(1.0) > 10.0 * tail(0.0).max(1e-5));
}
