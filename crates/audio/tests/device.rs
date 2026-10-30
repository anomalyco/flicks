//! Manual check that sound reaches the default output device: a 440 Hz tone in the centre of a
//! 5.1 source at 44.1 kHz (remixed and resampled to the device), with the reported clock.
//! `cargo test -p flicks-audio --test device -- --ignored --nocapture`

#![cfg(target_os = "macos")]

use std::time::{Duration, Instant};

use flicks_audio::{Channel, Output, layout};

#[test]
#[ignore = "plays a tone on the default output device"]
fn test_tone() {
    let mut out = Output::open().unwrap();
    println!("device: {} Hz, {} channels", out.rate(), out.channels());
    let rate = 44_100;
    let order: &[Channel] = layout::ac3(7, true).unwrap();
    let c = order.iter().position(|&ch| ch == Channel::C).unwrap();
    let pts0 = 10.0;
    for second in 0..2 {
        let mut samples = vec![0f32; rate * order.len()];
        for (i, frame) in samples.chunks_exact_mut(order.len()).enumerate() {
            let t = (second * rate + i) as f64 / rate as f64;
            frame[c] = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() as f32;
        }
        out.push(pts0 + second as f64, rate as u32, order, &samples).unwrap();
    }
    let start = Instant::now();
    let mut first = None;
    while start.elapsed() < Duration::from_millis(1500) {
        if let Some(p) = out.position() {
            first.get_or_insert((start.elapsed(), p));
        }
        std::thread::sleep(Duration::from_millis(250));
        println!(
            "{:>6.3} s: position {:?}, latency {:?}",
            start.elapsed().as_secs_f64(),
            out.position(),
            out.latency()
        );
    }
    let (t0, p0) = first.unwrap();
    let p1 = out.position().unwrap();
    let advanced = p1 - p0;
    let wall = (start.elapsed() - t0).as_secs_f64();
    println!("clock advanced {advanced:.3} s over {wall:.3} s wall");
    assert!(out.latency() > Duration::ZERO);
    assert!((pts0..pts0 + 2.0).contains(&p1));
    assert!((advanced - wall).abs() < 0.02, "clock {advanced:.3} s vs wall {wall:.3} s");
}
