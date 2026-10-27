use super::*;

const RATE: u32 = 48000;

fn render(piano: &mut EPiano, frames: usize) -> Vec<[f32; 2]> {
    let mut out = vec![[0.0; 2]; frames];
    piano.render(&mut out);
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

/// A piano with every effect off, so what is measured is the tine and the pickup.
fn bare() -> EPiano {
    let mut piano = EPiano::new(RATE);
    piano.set_bark(0.0);
    piano.set_chorus(0.0);
    piano.set_echo_level(0.0);
    piano.set_volume(1.0);
    piano
}

#[test]
fn silent_until_a_key_is_struck() {
    let mut piano = EPiano::new(RATE);
    assert!(render(&mut piano, 4800).iter().all(|f| f[0] == 0.0 && f[1] == 0.0));
}

#[test]
fn a_note_rings_at_its_pitch() {
    let mut piano = bare();
    piano.note_on(69, 100);
    render(&mut piano, 2400);
    let out = render(&mut piano, 9600);
    let (wanted, other) = (component(&out, 440.0), component(&out, 523.0));
    assert!(wanted > 0.05 && wanted > 20.0 * other, "{wanted} {other}");
}

#[test]
fn a_hard_hit_is_much_brighter_than_a_soft_one() {
    let ratio = |velocity| {
        let mut piano = bare();
        piano.set_model(Model::Bar);
        piano.set_bell(1.0);
        piano.note_on(60, velocity);
        let out = render(&mut piano, 4800);
        // The cantilever's second resonance, 6.267 times the fundamental.
        component(&out[0..1200], 261.63 * 6.267) / component(&out, 261.63)
    };
    let (soft, hard) = (ratio(30), ratio(127));
    assert!(hard > 4.0 * soft, "{soft} {hard}");
}

#[test]
fn the_bell_control_scales_the_high_resonances() {
    let high = |bell| {
        let mut piano = bare();
        piano.set_model(Model::Bar);
        piano.set_bell(bell);
        piano.note_on(60, 110);
        component(&render(&mut piano, 1200), 261.63 * 6.267)
    };
    let (bar, bell) = (high(0.0), high(1.0));
    assert!(bell > 0.01 && bell > 20.0 * bar, "{bar} {bell}");
}

#[test]
fn bark_adds_harmonics() {
    let ratio = |bark| {
        let mut piano = bare();
        piano.set_bark(bark);
        piano.note_on(48, 110);
        render(&mut piano, 2400);
        let out = render(&mut piano, 9600);
        component(&out, 392.45) / component(&out, 130.81)
    };
    assert!(ratio(1.0) > 0.03 && ratio(1.0) > 4.0 * ratio(0.0), "{} {}", ratio(0.0), ratio(1.0));
}

#[test]
fn the_damper_stops_a_released_note_and_the_pedal_holds_it() {
    let tail = |pedal| {
        let mut piano = bare();
        piano.set_sustain(pedal);
        piano.note_on(60, 100);
        render(&mut piano, 4800);
        piano.note_off(60);
        render(&mut piano, 4800);
        rms(&render(&mut piano, 4800))
    };
    assert!(tail(false) < 1e-4, "{}", tail(false));
    assert!(tail(true) > 0.02, "{}", tail(true));
}

#[test]
fn notes_decay_and_the_slider_sets_how_fast() {
    let level = |decay| {
        let mut piano = bare();
        piano.set_decay(decay);
        piano.note_on(60, 100);
        render(&mut piano, RATE as usize * 3);
        rms(&render(&mut piano, 4800))
    };
    assert!(level(0.0) < 0.2 * level(1.0), "{} {}", level(0.0), level(1.0));
}

#[test]
fn more_notes_than_voices_steal_without_trouble_and_stay_bounded() {
    for rate in [8000, 44100, 48000, 96000, 192000] {
        let mut piano = EPiano::new(rate);
        piano.set_bark(1.0);
        piano.set_drive(1.0);
        piano.set_phaser(1.0);
        piano.set_tremolo_depth(1.0);
        piano.set_echo_feedback(1.0);
        piano.set_mod_wheel(1.0);
        for note in LOWEST..=HIGHEST {
            piano.note_on(note, 127);
            let mut out = vec![[0.0; 2]; 256];
            piano.render(&mut out);
            assert!(out.iter().all(|f| f[0].abs() <= 1.0 && f[1].abs() <= 1.0 && f[0].is_finite() && f[1].is_finite()), "{rate} {note}");
        }
        assert!(piano.held_count() <= VOICES);
        let mut out = vec![[0.0; 2]; rate as usize];
        piano.render(&mut out);
        assert!(out.iter().all(|f| f[0].is_finite() && f[1].is_finite()), "{rate}");
    }
}

#[test]
fn midi_notes_pedal_bend_and_wheel() {
    let mut piano = EPiano::new(RATE);
    assert!(piano.midi(&[0x90, 60, 100]));
    assert!(piano.is_held(60));
    assert!(piano.midi(&[0x90, 60, 0]));
    assert!(!piano.is_held(60));
    assert!(piano.midi(&[0xb0, 64, 127]) && piano.sustain());
    assert!(piano.midi(&[0xe0, 0x7f, 0x7f]));
    assert!(piano.pitch_bend() > 0.99);
    assert!(piano.midi(&[0xe0, 0x00, 0x40]));
    assert_eq!(piano.pitch_bend(), 0.0);
    piano.set_echo_feedback(0.2);
    assert!(piano.midi(&[0xb0, 1, 127]));
    assert!(piano.echo().intensity() > 0.9);
    piano.set_mod_target(ModTarget::Tremolo);
    assert!((piano.echo().intensity() - 0.2).abs() < 1e-4);
    assert!(piano.midi(&[0xc0, 2]));
    assert_eq!(piano.model(), Model::Fm);
    assert!(!piano.midi(&[0xd0, 5]));
}

#[test]
fn the_tape_echo_repeats_a_struck_note() {
    let tail = |level: f32| {
        let mut piano = bare();
        piano.set_echo_level(level);
        piano.set_echo_repeat(1.0);
        piano.echo_mut().set_heads(tape_echo::HEAD3);
        piano.note_on(72, 110);
        render(&mut piano, 4800);
        piano.note_off(72);
        // The damper takes the note down within 90 ms, so what is heard then is head 3's echo (150 ms behind the tape).
        render(&mut piano, 4320);
        rms(&render(&mut piano, 2400))
    };
    assert!(tail(0.0) < 1e-4);
    assert!(tail(1.0) > 5e-3, "{}", tail(1.0));
}

#[test]
fn every_model_rings_at_the_pitch_dies_away_and_the_tine_is_less_bell_than_the_bar() {
    let bell_share = |model| {
        let mut piano = bare();
        piano.set_model(model);
        piano.set_bell(1.0);
        piano.note_on(60, 110);
        let early = render(&mut piano, 2400);
        let out = render(&mut piano, 9600);
        let (wanted, other) = (component(&out, 261.63), component(&out, 330.0));
        assert!(wanted > 0.03 && wanted > 10.0 * other, "{model:?} {wanted} {other}");
        piano.note_off(60);
        render(&mut piano, 9600);
        assert!(rms(&render(&mut piano, 4800)) < 1e-4, "{model:?}");
        component(&early[0..600], 261.63 * 6.267) / component(&early, 261.63)
    };
    let (bar, tine) = (bell_share(Model::Bar), bell_share(Model::Tine));
    for model in [Model::Fm, Model::Hybrid, Model::Metal, Model::Chime] {
        bell_share(model);
    }
    assert!(tine < 0.5 * bar, "{bar} {tine}");
}

#[test]
fn the_fm_models_are_brighter_when_struck_harder_and_stay_bounded() {
    for model in [Model::Fm, Model::Hybrid, Model::Metal] {
        let sidebands = |velocity| {
            let mut piano = bare();
            piano.set_model(model);
            piano.set_bell(1.0);
            piano.note_on(60, velocity);
            let out = render(&mut piano, 1200);
            assert!(out.iter().all(|f| f[0].is_finite() && f[0].abs() <= 1.0));
            component(&out, 261.63 * 3.0) / component(&out, 261.63)
        };
        assert!(sidebands(127) > 2.0 * sidebands(30), "{model:?}");
    }
}

#[test]
fn detune_sets_how_fast_a_held_note_beats() {
    let swing = |detune: f32| {
        let mut piano = bare();
        piano.set_model(Model::Tine);
        piano.set_detune(detune);
        piano.note_on(60, 100);
        render(&mut piano, 4800);
        // The level of a note over 1 s in 50 ms steps: the decay is a straight line in dB, beating bends it.
        let out = render(&mut piano, RATE as usize);
        let db: Vec<f32> = out.chunks(2400).map(|chunk| rms(chunk).ln()).collect();
        db.windows(3).map(|w| (w[0] - 2.0 * w[1] + w[2]).abs()).fold(0.0, f32::max)
    };
    assert!(swing(0.0) < 0.03 && swing(0.6) > 0.1, "{} {}", swing(0.0), swing(0.6));
}

#[test]
fn a_hard_chord_at_full_bark_does_not_clip() {
    for model in Model::ALL {
        let mut piano = EPiano::new(RATE);
        piano.set_model(model);
        piano.set_bark(1.0);
        piano.set_bell(1.0);
        for note in [36, 48, 55, 60, 64, 67, 72, 76] {
            piano.note_on(note, 127);
        }
        let peak = render(&mut piano, 9600).iter().fold(0.0f32, |peak, f| peak.max(f[0].abs()).max(f[1].abs()));
        assert!(peak < 0.98, "{model:?} {peak}");
    }
}

#[test]
fn the_chime_keeps_its_overtones_high_on_low_notes() {
    let mut piano = bare();
    piano.set_model(Model::Chime);
    piano.set_bell(1.0);
    piano.note_on(48, 120);
    let out = render(&mut piano, 4800);
    // C3 is 130.8 Hz: the bell 6.267x is 820 Hz and would be a wooden tock, so it is lifted to 3.3 kHz.
    assert!(component(&out, 130.81 * 6.267 * 4.0) > 20.0 * component(&out, 130.81 * 6.267));
}

#[test]
fn only_a_hard_hit_barks() {
    // A tine at C3 through its pickup at full Bark: the harmonics grow far faster than the swing.
    let rasp = |swing: f32| {
        let mut pickup = Pickup::new(130.81, REACH / 0.32, RATE as f32);
        let out: Vec<[f32; 2]> = (0..9600)
            .map(|n| {
                let y = pickup.tick(swing * (2.0 * std::f32::consts::PI * 130.81 * n as f32 / RATE as f32).sin(), 1.0);
                [y, y]
            })
            .collect();
        // Harmonics 4-9 (a soft note has a second and a third, a hard one spreads out).
        (4..=9).map(|n| component(&out[2400..], 130.81 * n as f32)).sum::<f32>() / component(&out[2400..], 130.81)
    };
    let (soft, hard) = (rasp(0.32 * 0.29), rasp(0.32));
    assert!(soft < 0.1 && hard > 10.0 * soft, "{soft} {hard}");
}

#[test]
fn touch_sets_the_velocity_curve() {
    let level = |touch| {
        let mut piano = bare();
        piano.set_touch(touch);
        piano.note_on(60, 64);
        component(&render(&mut piano, 4800), 261.63)
    };
    assert!(level(0.0) > 2.0 * level(1.0), "{} {}", level(0.0), level(1.0));
}

/// The `n`th harmonic of C2 (65.41 Hz) relative to the fundamental, in `frames`.
fn harmonic(frames: &[[f32; 2]], n: u32) -> f32 {
    component(frames, 65.41 * n as f32) / component(frames, 65.41)
}

#[test]
fn the_vowel_moves_the_formants() {
    let open = |vowel| {
        let mut piano = bare();
        piano.set_model(Model::Tine);
        piano.set_bark(1.0);
        piano.set_glide(0.0);
        piano.set_vowel(vowel);
        piano.note_on(36, 120);
        render(&mut piano, 2400);
        let out = render(&mut piano, 9600);
        // Near "a"'s first formant (11 x 65.41 = 720 Hz) against near "u"'s (5 x 65.41 = 327 Hz).
        harmonic(&out, 11) / harmonic(&out, 5)
    };
    let (u, a) = (open(0.0), open(1.0));
    assert!(a > 3.0 * u, "{u} {a}");
}

/// How open the vowel sounds in `frames` of C2: harmonics 10-13 (650-850 Hz, an "a") over 4-7 (260-460 Hz, an "o").
fn openness(frames: &[[f32; 2]]) -> f32 {
    let band = |range: core::ops::RangeInclusive<u32>| range.map(|n| component(frames, 65.41 * n as f32)).sum::<f32>();
    band(10..=13) / band(4..=7)
}

#[test]
fn a_hard_hit_opens_the_vowel_and_it_closes_as_the_note_dies() {
    let early_late = |glide| {
        let mut piano = bare();
        piano.set_model(Model::Tine);
        piano.set_bark(1.0);
        piano.set_vowel(0.3);
        piano.set_glide(glide);
        piano.note_on(36, 127);
        let out = render(&mut piano, 33600);
        (openness(&out[..2400]), openness(&out[28800..]))
    };
    let ((still, _), (opened, closed)) = (early_late(0.0), early_late(1.0));
    assert!(opened > 2.0 * still && opened > 3.0 * closed, "{still} {opened} {closed}");
}

#[test]
fn the_old_sweep_is_still_there() {
    let mut piano = bare();
    piano.set_model(Model::Metal);
    piano.set_voicing(Voicing::Sweep);
    piano.note_on(48, 110);
    let out = render(&mut piano, 4800);
    assert!(component(&out, 130.81) > 0.05 && out.iter().all(|f| f[0].abs() < 0.98));
}



#[test]
fn q_sharpens_the_formants_and_range_moves_them_up() {
    let peak = |q, range| {
        let mut piano = bare();
        piano.set_model(Model::Metal);
        piano.set_bark(1.0);
        piano.set_glide(0.0);
        piano.set_vowel(1.0);
        piano.set_q(q);
        piano.set_range(range);
        piano.note_on(36, 120);
        render(&mut piano, 2400);
        let out = render(&mut piano, 9600);
        assert!(out.iter().all(|f| f[0].abs() < 0.98));
        // The first formant at full openness: about 850 Hz (h13) by default, about 1.5 kHz (h23) at Range 0.65.
        (harmonic(&out, 13) / harmonic(&out, 1), harmonic(&out, 23) / harmonic(&out, 13))
    };
    let ((soft, _), (sharp, _)) = (peak(0.0, 0.25), peak(1.0, 0.25));
    assert!(sharp > 3.0 * soft, "{soft} {sharp}");
    let ((_, low), (_, high)) = (peak(0.6, 0.25), peak(0.6, 0.65));
    assert!(high > 4.0 * low, "{low} {high}");
}

#[test]
fn the_wah_pedal_opens_a_ringing_note() {
    let mut piano = bare();
    piano.set_model(Model::Metal);
    piano.set_bark(1.0);
    piano.set_glide(0.0);
    piano.set_vowel(0.0);
    piano.note_on(36, 120);
    render(&mut piano, 2400);
    let closed = openness(&render(&mut piano, 4800));
    assert!(piano.midi(&[0xb0, 11, 100]));
    render(&mut piano, 480);
    let pedal = openness(&render(&mut piano, 4800));
    piano.midi(&[0xb0, 11, 0]);
    piano.set_mod_target(ModTarget::All);
    piano.set_mod_wheel(0.79);
    render(&mut piano, 480);
    let all = openness(&render(&mut piano, 4800));
    piano.set_mod_target(ModTarget::Wah);
    render(&mut piano, 480);
    let wheel = openness(&render(&mut piano, 4800));
    assert!(pedal > 3.0 * closed && wheel > 3.0 * all, "{closed} {pedal} {all} {wheel}");
}

#[test]
fn every_model_plays_soft() {
    for model in Model::ALL {
        let level = |velocity| {
            let mut piano = bare();
            piano.set_model(model);
            piano.note_on(60, velocity);
            rms(&render(&mut piano, 4800))
        };
        assert!(level(10) < 0.05 * level(127), "{model:?} {} {}", level(10), level(127));
    }
}

#[test]
fn soft_lifts_the_velocity_floor() {
    let level = |soft, velocity| {
        let mut piano = bare();
        piano.set_model(Model::Metal);
        piano.set_soft(soft);
        piano.note_on(60, velocity);
        assert_eq!(piano.last_velocity(), velocity);
        rms(&render(&mut piano, 4800))
    };
    // With the floor at 80, velocity 80 is as soft as velocity 1 without it, and 127 is still the loudest.
    assert!(level(0.8, 80) < 1.5 * level(0.0, 1) && level(0.8, 80) < 0.05 * level(0.0, 80));
    assert!((level(0.8, 127) - level(0.0, 127)).abs() < 0.01 * level(0.0, 127));
}

#[test]
fn the_softest_hit_on_every_key_plays() {
    // A soft low note's filter ceiling is below 600 Hz: `clamp(600, top)` used to panic (min > max) with Soft raised.
    for model in Model::ALL {
        for soft in [0.0, 0.5, 1.0] {
            let mut piano = bare();
            piano.set_model(model);
            piano.set_soft(soft);
            for note in 21..=108 {
                for velocity in [1, 40, 80, 127] {
                    piano.note_on(note, velocity);
                }
            }
            assert!(render(&mut piano, 480).iter().all(|f| f[0].is_finite() && f[1].is_finite()), "{model:?} soft {soft}");
        }
    }
}
