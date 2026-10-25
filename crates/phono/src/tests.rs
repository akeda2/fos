use super::*;

fn by_id(id: &str) -> usize {
    find(id).unwrap_or_else(|| panic!("no curve {id}"))
}

#[test]
fn riaa_matches_the_published_values() {
    let riaa = &CURVES[curves::RIAA];
    for (hz, db) in [(20.0, 19.27), (50.0, 16.95), (100.0, 13.09), (500.0, 2.65), (1000.0, 0.0), (2122.0, -2.87), (5000.0, -8.21), (10000.0, -13.73), (20000.0, -19.62)] {
        let got = curve_db(riaa, hz);
        assert!((got - db).abs() < 0.1, "{hz} Hz: {got} dB, expected {db}");
    }
}

#[test]
fn every_curve_matches_the_table_at_50_hz_and_10_khz() {
    // (id, dB at 50 Hz, dB at 10 kHz) from the Graumann table; the table rounds to 0.1 dB and some entries are computed
    // from a nominal setting, so half a dB is the tolerance (1 dB at 50 Hz: the table is rounded, and the curves without a shelf get a 10 Hz pole here).
    for (id, bass, treble) in [
        ("riaa", 16.9, -13.7), ("columbia-lp", 13.6, -15.5), ("nab", 17.5, -15.6), ("nab-1942", 20.5, -15.6), ("aes", 18.1, -12.3),
        ("london-lp", 12.5, -10.9), ("teldec", 16.5, -10.9), ("rca-lp", 19.7, -13.7), ("rca-45", 22.6, -14.9),
        ("iec-78", 14.0, -10.5), ("ccir-78", 17.0, -10.5), ("decca-78", 15.4, -5.7), ("columbia-78", 16.7, -15.0),
        ("rca-78", 22.2, -9.9), ("euro-78", 14.4, 0.0), ("blumlein-300", 16.0, 0.0), ("flat-500", 19.0, -1.0),
        ("american-78-6", 14.0, -6.0), ("american-78-8", 14.0, -8.0),
    ] {
        let curve = &CURVES[by_id(id)];
        let (b, t) = (curve_db(curve, 50.0), curve_db(curve, 10000.0));
        assert!((b - bass).abs() < 1.0, "{id} at 50 Hz: {b} dB, table {bass}");
        assert!((t - treble).abs() < 0.5, "{id} at 10 kHz: {t} dB, table {treble}");
    }
}

#[test]
fn iec_cuts_the_bottom_octave_only() {
    let (riaa, iec) = (&CURVES[curves::RIAA], &CURVES[by_id("iec")]);
    assert!((curve_db(iec, 20.0) - curve_db(riaa, 20.0) + 3.0).abs() < 0.2);
    assert!((curve_db(iec, 1000.0) - curve_db(riaa, 1000.0)).abs() < 0.01);
    assert!((curve_db(iec, 200.0) - curve_db(riaa, 200.0)).abs() < 0.05);
}

#[test]
fn the_digital_curve_follows_the_analogue_one() {
    for rate in [44100, 48000, 96000] {
        for (index, curve) in CURVES.iter().enumerate() {
            let mut eq = Phono::new(rate);
            eq.set_curve(index);
            let limit = 16000.0;
            let mut hz = 20.0;
            while hz <= limit {
                let (digital, analogue) = (eq.response_db(hz), curve_db(curve, hz));
                assert!((digital - analogue).abs() < 0.25, "{} at {rate}: {hz} Hz digital {digital} analogue {analogue}", curve.id);
                hz *= 1.26;
            }
        }
    }
}

fn tone(eq: &mut Phono, hz: f32, rate: u32, amplitude: f32) -> f32 {
    let n = rate as usize;
    let mut left: Vec<f32> = (0..n).map(|i| amplitude * dsp::math::sin(2.0 * core::f32::consts::PI * hz * i as f32 / rate as f32)).collect();
    let mut right = left.clone();
    eq.process(&mut left, &mut right);
    left[n / 2..].iter().fold(0.0f32, |m, v| m.max(v.abs())) / amplitude
}

#[test]
fn a_tone_comes_out_at_the_curve_gain() {
    let mut eq = Phono::new(48000);
    for (hz, db) in [(1000.0, 0.0), (100.0, 13.09), (5000.0, -8.2)] {
        eq.reset();
        let gain = tone(&mut eq, hz, 48000, 0.01);
        let got = 20.0 * gain.log10();
        assert!((got - db).abs() < 0.5, "{hz} Hz: {got} dB, expected {db}");
    }
}

#[test]
fn trim_scales_the_output() {
    let mut eq = Phono::new(48000);
    eq.set_trim_db(-6.0);
    let gain = tone(&mut eq, 1000.0, 48000, 0.5);
    assert!((gain - 0.5012).abs() < 0.01, "{gain}");
}

#[test]
fn mono_sums_the_channels_and_polarity_flips_them() {
    let mut eq = Phono::new(48000);
    eq.set_curve(curves::FLAT);
    let (mut l, mut r) = (vec![0.5f32; 64], vec![-0.1f32; 64]);
    eq.process(&mut l, &mut r);
    assert!((l[63] - 0.5).abs() < 1e-5 && (r[63] + 0.1).abs() < 1e-5);

    eq.set_mono(true);
    let (mut l, mut r) = (vec![0.5f32; 64], vec![-0.1f32; 64]);
    eq.process(&mut l, &mut r);
    assert!((l[63] - 0.2).abs() < 1e-5 && (r[63] - 0.2).abs() < 1e-5);

    eq.set_mono(false);
    for (polarity, expect) in [(Polarity::InvertLeft, (-0.5, -0.1)), (Polarity::InvertRight, (0.5, 0.1)), (Polarity::InvertBoth, (-0.5, 0.1))] {
        eq.set_polarity(polarity);
        let (mut l, mut r) = (vec![0.5f32; 8], vec![-0.1f32; 8]);
        eq.process(&mut l, &mut r);
        assert!((l[7] - expect.0).abs() < 1e-5 && (r[7] - expect.1).abs() < 1e-5, "{polarity:?}");
    }
}

#[test]
fn mono_cancels_an_out_of_phase_rumble() {
    let mut eq = Phono::new(48000);
    eq.set_mono(true);
    let mut left: Vec<f32> = (0..4800).map(|i| 0.1 * dsp::math::sin(i as f32 * 0.01)).collect();
    let mut right: Vec<f32> = left.iter().map(|v| -v).collect();
    eq.process(&mut left, &mut right);
    assert!(left.iter().all(|v| v.abs() < 1e-6));
}

#[test]
fn rumble_filter_cuts_below_its_corner() {
    let mut eq = Phono::new(48000);
    eq.set_curve(curves::FLAT);
    eq.set_rumble(Rumble::Steep(40.0));
    assert!(eq.response_db(20.0) < -10.0, "{}", eq.response_db(20.0));
    assert!((eq.response_db(40.0) + 3.0).abs() < 0.3);
    assert!(eq.response_db(1000.0).abs() < 0.05);
    eq.set_rumble(Rumble::Off);
    assert!(eq.response_db(20.0).abs() < 0.01);
}

#[test]
fn peaks_report_input_and_output_and_reset() {
    let mut eq = Phono::new(48000);
    let mut l = vec![0.3f32; 2000];
    let mut r = vec![0.0f32; 2000];
    eq.process(&mut l, &mut r);
    let p = eq.take_peaks();
    assert!((p.input[0] - 0.3).abs() < 1e-6 && p.input[1] == 0.0);
    assert!(p.output[0] > 0.3, "DC-ish bass is boosted");
    assert_eq!(eq.take_peaks().input, [0.0, 0.0]);
}

#[test]
fn ids_are_unique_and_found() {
    for (i, a) in CURVES.iter().enumerate() {
        assert_eq!(find(a.id), Some(i));
        assert_eq!(find(&a.id.to_uppercase().replace('-', "_")), Some(i));
    }
    assert_eq!(find("nonsense"), None);
}

#[test]
fn bass_boost_peaks_where_asked_and_leaves_the_midrange() {
    for (hz, q) in [(60.0, 0.3), (100.0, 0.7), (150.0, 3.0)] {
        let mut eq = Phono::new(48000);
        eq.set_curve(curves::FLAT);
        eq.set_bass_boost(BassBoost { gain_db: 9.0, hz, q });
        assert!((eq.response_db(hz) - 9.0).abs() < 0.05, "{hz} Hz Q {q}: {}", eq.response_db(hz));
        assert!(eq.response_db(2000.0).abs() < 0.3 || q < 0.5, "{}", eq.response_db(2000.0));
        assert!(eq.response_db(10000.0).abs() < 0.5);
    }
    let (mut narrow, mut wide) = (Phono::new(48000), Phono::new(48000));
    narrow.set_curve(curves::FLAT);
    wide.set_curve(curves::FLAT);
    narrow.set_bass_boost(BassBoost { gain_db: 12.0, hz: 100.0, q: 3.0 });
    wide.set_bass_boost(BassBoost { gain_db: 12.0, hz: 100.0, q: 0.3 });
    assert!(wide.response_db(300.0) > narrow.response_db(300.0) + 3.0);
}

#[test]
fn bass_boost_is_clamped_and_off_at_zero() {
    let mut eq = Phono::new(48000);
    eq.set_curve(curves::FLAT);
    eq.set_bass_boost(BassBoost { gain_db: 40.0, hz: 10.0, q: 100.0 });
    assert_eq!(eq.bass_boost(), BassBoost { gain_db: 12.0, hz: 60.0, q: 3.0 });
    eq.set_bass_boost(BassBoost::OFF);
    assert!(eq.response_db(100.0).abs() < 1e-6);
}

#[test]
fn frames_and_slices_agree_and_buttons_fit() {
    let mut a = Phono::new(48000);
    let mut b = Phono::new(48000);
    let mut frames: Vec<[f32; 2]> = (0..500).map(|i| [dsp::math::sin(i as f32 * 0.1), dsp::math::sin(i as f32 * 0.13)]).collect();
    let (mut l, mut r): (Vec<f32>, Vec<f32>) = frames.iter().map(|f| (f[0], f[1])).unzip();
    a.process_frames(&mut frames);
    b.process(&mut l, &mut r);
    assert!(frames.iter().zip(l.iter().zip(r.iter())).all(|(f, (l, r))| f[0] == *l && f[1] == *r));
    assert!(CURVES.iter().all(|c| c.short.len() <= 8 && !c.short.is_empty()));
}
