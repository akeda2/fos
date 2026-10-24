use super::*;

const RATE: u32 = 48000;

fn quiet(rate: u32) -> TapeEcho {
    let mut echo = TapeEcho::new(rate);
    echo.set_dry(0.0);
    echo.set_reverb(0.0);
    echo.set_wow(0.0);
    echo.set_intensity(0.0);
    echo.set_heads(HEAD1);
    echo.set_echo_level(1.0);
    echo
}

fn rms(values: &[f32]) -> f32 {
    (values.iter().map(|v| v * v).sum::<f32>() / values.len() as f32).sqrt()
}

fn sine(hz: f32, n: usize, rate: u32) -> f32 {
    (2.0 * core::f32::consts::PI * hz * n as f32 / rate as f32).sin()
}

/// The left output for `frames` of input, from `input(n)`.
fn run(echo: &mut TapeEcho, frames: usize, input: impl Fn(usize) -> f32) -> Vec<f32> {
    (0..frames).map(|n| echo.process(input(n), input(n)).0).collect()
}

fn peak_near(data: &[f32], at: usize, around: usize) -> (usize, f32) {
    (at.saturating_sub(around)..(at + around).min(data.len())).map(|i| (i, data[i].abs())).fold((0, 0.0), |best, v| if v.1 > best.1 { v } else { best })
}

#[test]
fn nothing_in_nothing_out_and_the_dry_signal_passes_unchanged() {
    let mut echo = TapeEcho::new(RATE);
    echo.set_heads(0);
    echo.set_reverb(0.0);
    echo.set_dry(1.0);
    let out = run(&mut echo, 2000, |n| sine(440.0, n, RATE));
    assert!(out.iter().enumerate().all(|(n, v)| (v - sine(440.0, n, RATE)).abs() < 1e-6));
    let mut echo = quiet(RATE);
    echo.set_heads(0);
    assert!(run(&mut echo, 4800, |n| sine(440.0, n, RATE)).iter().all(|v| *v == 0.0));
}

#[test]
fn an_echo_comes_back_after_the_delay_and_the_heads_are_spaced_one_two_three() {
    let mut echo = quiet(RATE);
    echo.set_delay(0.2);
    run(&mut echo, RATE as usize, |_| 0.0);
    echo.set_heads(HEAD1 | HEAD2 | HEAD3);
    let out = run(&mut echo, RATE as usize * 2, |n| if n == 100 { 1.0 } else { 0.0 });
    for head in 1..=3 {
        let expected = 100 + head * 9600;
        let (at, level) = peak_near(&out, expected, 40);
        assert!(level > 0.05 && (at as i32 - expected as i32).abs() <= 25, "head {head}: {at} vs {expected}, {level}");
    }
    assert!(rms(&out[..9000]) < 1e-4, "nothing before the first head");
}

#[test]
fn intensity_repeats_and_each_repeat_is_quieter_and_darker() {
    let mut echo = quiet(RATE);
    echo.set_delay(0.1);
    echo.set_intensity(0.6);
    run(&mut echo, RATE as usize, |_| 0.0);
    let out = run(&mut echo, RATE as usize, |n| if n == 0 { 1.0 } else { 0.0 });
    let level = |k: usize| peak_near(&out, k * 4800, 60).1;
    assert!(level(2) < level(1) && level(2) > 0.3 * level(1), "{} {}", level(1), level(2));
    assert!(level(3) < level(2) && level(3) > 0.2 * level(2));

    let amplitude = |hz: f32| {
        let mut echo = quiet(RATE);
        echo.set_delay(0.1);
        echo.set_saturation(0.0);
        let out = run(&mut echo, 24000, |n| 0.2 * sine(hz, n, RATE));
        rms(&out[12000..]) * core::f32::consts::SQRT_2
    };
    assert!(amplitude(9000.0) < 0.7 * amplitude(500.0));
}

#[test]
fn the_repeat_rate_glides_the_pitch() {
    let mut echo = quiet(RATE);
    echo.set_repeat_rate(0.0);
    run(&mut echo, RATE as usize * 2, |n| sine(1000.0, n, RATE));
    echo.set_repeat_rate(1.0);
    let out = run(&mut echo, 4800, |n| sine(1000.0, n, RATE));
    let crossings = out[960..3360].windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    // 50 ms of a steady 1 kHz would be 50 rising crossings; the heads moving towards the record head squeeze the waves.
    assert!(crossings >= 56, "{crossings}");
}

#[test]
fn freeze_holds_the_loop_while_a_normal_loop_dies() {
    let hold = |frozen: bool| {
        let mut echo = quiet(RATE);
        echo.set_delay(0.1);
        echo.set_intensity(0.5);
        run(&mut echo, RATE as usize, |n| sine(1000.0, n, RATE));
        echo.set_freeze(frozen);
        let out = run(&mut echo, RATE as usize * 6, |_| 0.0);
        (rms(&out[RATE as usize / 2..RATE as usize]), rms(&out[RATE as usize * 5..]))
    };
    let (early, late) = hold(true);
    assert!(early > 0.1 && late > 0.6 * early, "{early} {late}");
    let (early, late) = hold(false);
    assert!(late < 0.01 * early.max(1e-3), "{early} {late}");
}

#[test]
fn full_intensity_with_everything_on_stays_bounded_at_every_rate() {
    for rate in [8000, 44100, 48000, 96000, 192000] {
        let mut echo = TapeEcho::new(rate);
        echo.set_heads(HEAD1 | HEAD2 | HEAD3);
        echo.set_intensity(1.0);
        echo.set_saturation(1.0);
        echo.set_reverb(1.0);
        echo.set_reverb_decay(1.0);
        echo.set_wow(1.0);
        echo.set_bass(1.0);
        echo.set_treble(1.0);
        echo.set_repeat_rate(0.3);
        for n in 0..rate as usize * 6 {
            let x = if n < rate as usize / 2 { 0.8 * sine(220.0, n, rate) } else { 0.0 };
            let (l, r) = echo.process(x, x);
            assert!(l.is_finite() && r.is_finite() && l.abs() < 12.0 && r.abs() < 12.0, "{rate} {n} {l} {r}");
        }
    }
}

#[test]
fn the_reverb_alone_makes_a_tail_without_echoes() {
    let mut echo = quiet(RATE);
    echo.set_heads(0);
    echo.set_reverb(1.0);
    let out = run(&mut echo, RATE as usize, |n| if n == 0 { 1.0 } else { 0.0 });
    assert!(rms(&out[2000..20000]) > 0.001);
}

#[test]
fn set_delay_round_trips_and_fit_delay_folds_into_range() {
    let mut echo = TapeEcho::new(RATE);
    for seconds in [0.06, 0.12, 0.3] {
        echo.set_delay(seconds);
        assert!((echo.delay() - seconds).abs() < 0.001, "{seconds} {}", echo.delay());
    }
    assert!((echo.fit_delay(0.6) - 0.3).abs() < 1e-6 && (echo.fit_delay(0.03) - 0.06).abs() < 1e-6);
    let high = TapeEcho::new(192000);
    assert!(high.delay_range().1 < 0.12);
}

#[test]
fn tap_tempo_averages_the_gaps_and_ignores_stale_and_bouncing_taps() {
    let mut tap = TapTempo::new();
    assert_eq!(tap.tap(1_000_000), None);
    assert_eq!(tap.tap(1_500_000), Some(0.5));
    assert_eq!(tap.tap(1_510_000), None);
    let average = tap.tap(2_100_000).unwrap();
    assert!((average - 0.55).abs() < 1e-4, "{average}");
    assert_eq!(tap.tap(9_000_000), None);
    assert_eq!(tap.tap(9_400_000), Some(0.4));
}
