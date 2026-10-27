use super::*;

const RATE: u32 = 48000;

fn sine_hz(hz: f32, n: usize) -> impl Iterator<Item = f32> {
    (0..n).map(move |i| math::sin(2.0 * core::f32::consts::PI * hz * i as f32 / RATE as f32))
}

fn rms(values: &[f32]) -> f32 {
    math::sqrt(values.iter().map(|v| v * v).sum::<f32>() / values.len() as f32)
}

#[test]
fn sine_table_is_a_sine() {
    let sine = Sine::new();
    assert!(sine.at(0).abs() < 1e-4);
    assert!((sine.at(1 << 30) - 1.0).abs() < 1e-4);
    assert!(sine.at(3 << 30) < -0.999);
    assert!((sine.at(phase_step(1.0, 8.0)) - 0.7071).abs() < 1e-3);
}

#[test]
fn delay_reads_back_interpolated() {
    let mut delay: Delay<16> = Delay::new();
    for i in 0..8 {
        delay.push(i as f32);
    }
    assert_eq!(delay.read(1.0), 7.0);
    assert_eq!(delay.read(3.0), 5.0);
    assert!((delay.read(2.5) - 5.5).abs() < 1e-6);
}

#[test]
fn overdrive_is_clean_at_zero_and_bounded() {
    assert_eq!(overdrive(0.3, 0.0), 0.3);
    assert!(overdrive(100.0, 1.0) <= 2.0);
}

#[test]
fn rotary_off_is_a_bypass_and_fast_modulates() {
    let mut rotary = Rotary::new(RATE);
    assert_eq!(rotary.process(0.5), (0.5, 0.5));
    rotary.set_mode(RotaryMode::Fast);
    let mut differ = 0.0;
    for x in sine_hz(2000.0, RATE as usize * 4) {
        let (l, r) = rotary.process(x);
        differ += (l - r).abs();
    }
    assert!(differ > 100.0);
}

#[test]
fn ensemble_off_is_a_bypass_and_on_stays_bounded() {
    let mut ensemble = Ensemble::new(RATE);
    assert_eq!(ensemble.process(0.25), (0.25, 0.25));
    ensemble.set_mix(1.0);
    let mut peak: f32 = 0.0;
    let mut left = std::vec::Vec::new();
    let mut right = std::vec::Vec::new();
    for x in sine_hz(440.0, RATE as usize * 2) {
        let (l, r) = ensemble.process(x);
        peak = peak.max(l.abs()).max(r.abs());
        left.push(l);
        right.push(r);
    }
    assert!(peak < 1.5, "peak {peak}");
    let tail = RATE as usize;
    assert!(rms(&left[tail..]) > 0.3 && rms(&right[tail..]) > 0.3);
    let difference: f32 = left[tail..].iter().zip(&right[tail..]).map(|(a, b)| (a - b).abs()).sum();
    assert!(difference > 100.0, "channels should differ");
}

#[test]
fn ensemble_is_silent_after_input_stops() {
    let mut ensemble = Ensemble::new(RATE);
    ensemble.set_mix(1.0);
    for x in sine_hz(440.0, 4800) {
        ensemble.process(x);
    }
    let mut last = (1.0, 1.0);
    for _ in 0..1000 {
        last = ensemble.process(0.0);
    }
    assert_eq!(last, (0.0, 0.0));
}

#[test]
fn phaser_off_is_a_bypass_and_keeps_level() {
    let mut phaser = Phaser::new(RATE);
    assert_eq!(phaser.process(0.5, -0.5), (0.5, -0.5));
    phaser.set_mix(1.0);
    let mut out = std::vec::Vec::new();
    for x in sine_hz(1000.0, RATE as usize * 3) {
        out.push(phaser.process(x, x).0);
    }
    assert!(out.iter().all(|v| v.is_finite() && v.abs() < 3.0));
    let level = rms(&out[RATE as usize..]);
    assert!(level > 0.1 && level < 1.5, "rms {level}");
}

#[test]
fn phaser_notch_moves_with_the_sweep() {
    let mut phaser = Phaser::new(RATE);
    phaser.set_mix(1.0);
    phaser.set_feedback(0.0);
    phaser.set_speed(2.0);
    let mut levels = std::vec::Vec::new();
    let signal: std::vec::Vec<f32> = sine_hz(800.0, RATE as usize * 2).collect();
    for block in signal.chunks(2400) {
        let out: std::vec::Vec<f32> = block.iter().map(|x| phaser.process(*x, *x).0).collect();
        levels.push(rms(&out));
    }
    let min = levels.iter().cloned().fold(f32::MAX, f32::min);
    let max = levels.iter().cloned().fold(0.0, f32::max);
    assert!(max > 1.5 * min, "sweep should change the level of a steady tone: {min} {max}");
}

#[test]
fn effects_work_at_every_rate() {
    for rate in [8000, 44100, 96000, 192000] {
        let mut ensemble = Ensemble::new(rate);
        ensemble.set_mix(1.0);
        let mut phaser = Phaser::new(rate);
        phaser.set_mix(1.0);
        let mut rotary = Rotary::new(rate);
        rotary.set_mode(RotaryMode::Slow);
        for i in 0..20000 {
            let x = math::sin(i as f32 * 0.05);
            let (l, r) = ensemble.process(x);
            let (l, r) = phaser.process(l, r);
            let (l, _) = rotary.process(l + r);
            assert!(l.is_finite() && l.abs() < 10.0);
        }
    }
}

#[test]
fn spring_off_is_a_bypass_and_an_impulse_rings_then_fades() {
    let mut spring = Spring::new(RATE);
    assert_eq!(spring.process(0.3, -0.2), (0.3, -0.2));
    spring.set_mix(1.0);
    spring.set_decay(0.5);
    let mut left = Vec::new();
    for n in 0..RATE as usize * 4 {
        let (l, r) = spring.process(if n == 0 { 1.0 } else { 0.0 }, 0.0);
        assert!(l.is_finite() && r.is_finite());
        left.push(l);
    }
    let early = rms(&left[1000..12000]);
    let late = rms(&left[RATE as usize * 3..]);
    assert!(early > 0.002 && late < 0.2 * early, "{early} {late}");
    // The first echo comes after one trip of the spring (29 ms), not at once.
    assert!(rms(&left[1..600]) < 0.05 * early.max(1e-6) + 1e-3);
}

#[test]
fn a_longer_decay_rings_longer() {
    let tail = |decay: f32| {
        let mut spring = Spring::new(RATE);
        spring.set_mix(1.0);
        spring.set_decay(decay);
        let mut out = Vec::new();
        for n in 0..RATE as usize * 2 {
            out.push(spring.process(if n == 0 { 1.0 } else { 0.0 }, 0.0).0);
        }
        rms(&out[RATE as usize * 3 / 2..])
    };
    assert!(tail(1.0) > 3.0 * tail(0.0));
}

#[test]
fn the_spring_is_dispersive_low_frequencies_arrive_later() {
    // The energy centroid of a windowed tone burst after the allpass chain, minus before it.
    let delay = |hz: f32| {
        let mut chain = spring::Dispersion::new(-0.65);
        let (mut before, mut after, mut weight) = (0.0f64, 0.0f64, 0.0f64);
        for n in 0..8192usize {
            let x = if n < 2048 {
                let window = 0.5 - 0.5 * math::cos(2.0 * core::f32::consts::PI * n as f32 / 2048.0);
                window * math::sin(2.0 * core::f32::consts::PI * hz * n as f32 / RATE as f32)
            } else {
                0.0
            };
            let y = chain.process(x);
            before += (x * x) as f64 * n as f64;
            after += (y * y) as f64 * n as f64;
            weight += (x * x) as f64;
        }
        ((after - before) / weight) as f32
    };
    let (low, high) = (delay(400.0), delay(4000.0));
    assert!(low > high + 50.0, "{low} {high}");
}

#[test]
fn a_loud_input_at_full_decay_stays_bounded() {
    let mut spring = Spring::new(RATE);
    spring.set_mix(1.0);
    spring.set_decay(1.0);
    for n in 0..RATE as usize * 5 {
        let x = 1.0 * math::sin(2.0 * core::f32::consts::PI * 300.0 * n as f32 / RATE as f32);
        let (l, r) = spring.process(x, x);
        assert!(l.abs() < 10.0 && r.abs() < 10.0, "{n}");
    }
}

/// Power in every bin of a 4096-point DFT that is not a harmonic of bin `k` (the folded-back part), for one period of
/// `render`'s periodic output.
fn inharmonic_power(k: usize, render: &mut dyn FnMut(f32) -> f32) -> f64 {
    const N: usize = 4096;
    let tone = |i: usize| (2.0 * std::f64::consts::PI * (k * i) as f64 / N as f64).sin() as f32 * 0.3;
    for i in 0..N {
        render(tone(i));
    }
    let out: Vec<f64> = (0..N).map(|i| render(tone(i)) as f64).collect();
    (1..N / 2)
        .filter(|bin| bin % k != 0)
        .map(|bin| {
            let (mut re, mut im) = (0.0, 0.0);
            for (i, v) in out.iter().enumerate() {
                let angle = 2.0 * std::f64::consts::PI * (bin * i) as f64 / N as f64;
                re += v * angle.cos();
                im -= v * angle.sin();
            }
            re * re + im * im
        })
        .sum()
}

#[test]
fn oversampled_overdrive_folds_far_less_and_keeps_the_level() {
    // 6996 Hz at 48 kHz: its 3rd, 5th, 7th ... harmonics are above Nyquist and fold into the band.
    let mut naive = |x| overdrive(x, 1.0);
    let mut over = Overdrive::new();
    let mut better = |x| over.process(x, 1.0);
    let before = inharmonic_power(597, &mut naive);
    let after = inharmonic_power(597, &mut better);
    assert!(after < before * 0.01, "folded power {after} vs {before}");

    let mut over = Overdrive::new();
    let mut gentle = |x| over.process(x, 0.001);
    let mut plain = |x| overdrive(x, 0.001);
    let tone = |i: usize| 0.05 * math::sin(2.0 * core::f32::consts::PI * 1000.0 * i as f32 / RATE as f32);
    let (mut a, mut b) = (Vec::new(), Vec::new());
    for i in 0..4800 {
        let (x, y) = (gentle(tone(i)), plain(tone(i)));
        if i >= 2400 {
            a.push(x);
            b.push(y);
        }
    }
    assert!((rms(&a) / rms(&b) - 1.0).abs() < 0.02);
}

#[test]
fn oversampled_overdrive_is_a_bypass_at_zero() {
    let mut over = Overdrive::new();
    assert_eq!(over.process(0.3, 0.0), 0.3);
}

#[test]
fn decimator_keeps_the_band_and_removes_what_the_base_rate_cannot_hold() {
    // At the 4x rate (192 kHz): 1 kHz must pass at unit gain, 40 kHz (above the base Nyquist) must be gone.
    let rms_of = |hz: f32| {
        let mut decimator = Decimator::new();
        let mut out = Vec::new();
        for n in 0..2400 {
            let quad = core::array::from_fn(|j| math::sin(2.0 * core::f32::consts::PI * hz * (n * 4 + j) as f32 / (4 * RATE) as f32));
            out.push(decimator.process(quad));
        }
        rms(&out[400..]) * core::f32::consts::SQRT_2
    };
    assert!((rms_of(1000.0) - 1.0).abs() < 0.02);
    assert!(rms_of(40000.0) < 0.01);
}
