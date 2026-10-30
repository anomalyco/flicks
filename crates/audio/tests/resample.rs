//! The resampler against computed sines: THD+N (residual after a least-squares fit at the tone's
//! frequency), the largest error against the ideal sine at the output rate, aliasing of a tone
//! above the output Nyquist frequency, chunked == one-shot, and cost.

use std::f64::consts::PI;

use flicks_audio::Resampler;

fn sine(rate: u32, freq: f64, frames: usize, channels: usize) -> Vec<f32> {
    (0..frames)
        .flat_map(|i| std::iter::repeat_n((0.5 * (2.0 * PI * freq * i as f64 / rate as f64).sin()) as f32, channels))
        .collect()
}

fn resample(from: u32, to: u32, input: &[f32], channels: usize) -> Vec<f32> {
    let mut r = Resampler::new(from, to, channels).unwrap();
    let mut out = Vec::new();
    r.process(input, &mut out);
    out
}

/// (THD+N dB, max |error| against the ideal sine) of channel 0 over the middle of the output.
fn measure(out: &[f32], channels: usize, rate: u32, freq: f64) -> (f64, f64) {
    let x: Vec<f64> = out.chunks_exact(channels).map(|f| f[0] as f64).collect();
    let range = rate as usize / 10..x.len() - rate as usize / 10;
    let w = 2.0 * PI * freq / rate as f64;
    // Least-squares fit a sin + b cos (the tone's frequency is exact, so this is a 2×2 system).
    let (mut ss, mut cc, mut sc, mut xs, mut xc) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for n in range.clone() {
        let (s, c) = (w * n as f64).sin_cos();
        (ss, cc, sc, xs, xc) = (ss + s * s, cc + c * c, sc + s * c, xs + x[n] * s, xc + x[n] * c);
    }
    let det = ss * cc - sc * sc;
    let (a, b) = ((xs * cc - xc * sc) / det, (xc * ss - xs * sc) / det);
    let (mut sig, mut res, mut max) = (0.0, 0.0, 0f64);
    for n in range {
        let (s, c) = (w * n as f64).sin_cos();
        let fit = a * s + b * c;
        sig += fit * fit;
        res += (x[n] - fit).powi(2);
        max = max.max((x[n] - 0.5 * s).abs());
    }
    (10.0 * (res / sig).log10(), max)
}

#[test]
fn sine_quality() {
    println!("{:<14} {:>8} {:>10} {:>12}", "rates", "tone Hz", "THD+N dB", "max |err|");
    for (from, to) in [(44_100, 48_000), (48_000, 44_100)] {
        for freq in [1_000.0, 10_000.0, 18_000.0] {
            let out = resample(from, to, &sine(from, freq, from as usize, 1), 1);
            assert!(out.len().abs_diff(to as usize) <= 40, "{from}->{to}: {} frames", out.len());
            let (thdn, max) = measure(&out, 1, to, freq);
            println!("{:<14} {freq:>8} {thdn:>10.1} {max:>12.2e}", format!("{from}->{to}"));
            let (thdn_limit, max_limit) = if freq <= 10_000.0 { (-100.0, 1e-4) } else { (-80.0, 2e-3) };
            assert!(thdn < thdn_limit && max < max_limit, "{from}->{to} {freq} Hz: THD+N {thdn:.1} dB, max {max:.2e}");
        }
    }
    // A 23 kHz tone has no place at 44.1 kHz: what aliases back (to 21.1 kHz) must be tiny.
    let out = resample(48_000, 44_100, &sine(48_000, 23_000.0, 48_000, 1), 1);
    let rms = (out[4410..out.len() - 4410].iter().map(|&v| (v as f64).powi(2)).sum::<f64>()
        / (out.len() - 8820) as f64)
        .sqrt();
    let alias = 20.0 * (rms / (0.5 / 2f64.sqrt())).log10();
    println!("48000->44100 23 kHz alias {alias:.1} dB");
    assert!(alias < -80.0, "alias {alias:.1} dB");
}

#[test]
fn chunked_matches_one_shot() {
    let input = sine(44_100, 997.0, 20_000, 2);
    let whole = resample(44_100, 48_000, &input, 2);
    let mut r = Resampler::new(44_100, 48_000, 2).unwrap();
    let mut chunked = Vec::new();
    for chunk in input.chunks(2 * 333) {
        r.process(chunk, &mut chunked);
    }
    assert_eq!(whole, chunked);
    r.reset();
    let mut again = Vec::new();
    r.process(&input, &mut again);
    assert_eq!(whole, again);
}

#[test]
fn rejects_unusable_ratios() {
    assert!(Resampler::new(0, 48_000, 2).is_err());
    assert!(Resampler::new(48_000, 48_001, 2).is_err());
    assert!(Resampler::new(48_000, 44_100, 9).is_err());
}

/// Cost per second of stereo audio (meaningful with `--release`; recorded in docs/perf.md).
#[test]
fn cost() {
    for (from, to) in [(44_100, 48_000), (48_000, 44_100)] {
        let input = sine(from, 1_000.0, from as usize * 10, 2);
        let mut r = Resampler::new(from, to, 2).unwrap();
        let mut out = Vec::with_capacity(to as usize * 21);
        let t = std::time::Instant::now();
        for chunk in input.chunks(2 * 1024) {
            r.process(chunk, &mut out);
        }
        let us = t.elapsed().as_secs_f64() * 1e6 / 10.0;
        println!("{from}->{to} stereo: {us:.0} µs per second of audio");
    }
}

/// Low rates up to the highest device rates: 22.05 → 192 kHz needs 1280 filter phases and
/// 11.025 → 192 kHz 2560.
#[test]
fn low_to_high_rates() {
    for from in [22_050, 11_025] {
        let to = 192_000;
        let out = resample(from, to, &sine(from, 1_000.0, from as usize, 1), 1);
        assert!(
            out.len().abs_diff(to as usize) <= 32 * to as usize / from as usize,
            "{from}->{to}: {} frames",
            out.len()
        );
        let (thdn, max) = measure(&out, 1, to, 1_000.0);
        println!("{from}->{to} 1000 Hz: THD+N {thdn:.1} dB, max |err| {max:.2e}");
        assert!(thdn < -100.0 && max < 1e-4, "{from}->{to}: THD+N {thdn:.1} dB, max {max:.2e}");
    }
}
