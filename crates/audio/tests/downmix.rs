//! 5.1 AAC, AC-3, E-AC-3 and Opus through our decoders and `flicks-audio`: the canonical layout
//! must match ffmpeg's 5.1 decode channel for channel, the stereo downmix must match ffmpeg's
//! (libswresample's BS.775 matrix applied to that decode), and per-channel tones must land where
//! BS.775 puts them. ffmpeg is an external oracle and fixture generator only.

use std::path::Path;
use std::process::Command;

use flicks_audio::{Channel, interleave, layout, mix};

const RATE: usize = 48_000;
const AMP: f32 = 0.2;
/// One tone per source channel, in ffmpeg's 5.1 order (FL FR FC LFE BL BR) = canonical order.
const TONES: [f64; 6] = [440.0, 660.0, 1000.0, 60.0, 1500.0, 2200.0];

fn ffmpeg(ff: &Path, args: &[&str]) {
    let out = Command::new(ff).args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).output().unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn read_f32(path: &Path) -> Vec<f32> {
    std::fs::read(path).unwrap().as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

fn write_f32(path: &Path, samples: &[f32]) {
    std::fs::write(path, samples.iter().flat_map(|v| v.to_le_bytes()).collect::<Vec<u8>>()).unwrap();
}

/// Amplitude of the `freq` component of `channel` over one second from 0.5 s (integer-Hz tones
/// sit exactly on a bin).
fn tone(interleaved: &[f32], channels: usize, channel: usize, freq: f64) -> f64 {
    let w = 2.0 * std::f64::consts::PI * freq / RATE as f64;
    let (mut re, mut im) = (0.0, 0.0);
    for i in 0..RATE {
        let x = interleaved[(RATE / 2 + i) * channels + channel] as f64;
        re += x * (w * i as f64).cos();
        im += x * (w * i as f64).sin();
    }
    2.0 * (re * re + im * im).sqrt() / RATE as f64
}

fn snr_db(reference: &[f32], test: &[f32]) -> f64 {
    let n = reference.len().min(test.len());
    let sig: f64 = reference[..n].iter().map(|&v| (v as f64).powi(2)).sum();
    let err: f64 = reference[..n].iter().zip(&test[..n]).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum();
    10.0 * (sig / err.max(1e-30)).log10()
}

fn ogg_packets(data: &[u8]) -> (Vec<Vec<u8>>, i64) {
    let (mut packets, mut cur, mut pos, mut granule) = (Vec::new(), Vec::new(), 0, 0i64);
    while pos + 27 <= data.len() && &data[pos..pos + 4] == b"OggS" {
        let g = i64::from_le_bytes(data[pos + 6..pos + 14].try_into().unwrap());
        let nseg = data[pos + 26] as usize;
        let mut body = pos + 27 + nseg;
        for &l in &data[pos + 27..pos + 27 + nseg] {
            cur.extend_from_slice(&data[body..body + l as usize]);
            body += l as usize;
            if l < 255 {
                packets.push(std::mem::take(&mut cur));
            }
        }
        granule = granule.max(g);
        pos = body;
    }
    (packets, granule)
}

/// Our decode: interleaved samples in the decoder's order, and that order.
fn decode(ext: &str, data: &[u8]) -> (Vec<f32>, &'static [Channel]) {
    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut append = |out: Vec<Vec<f32>>| {
        planar.resize(out.len().max(planar.len()), Vec::new());
        for (p, o) in planar.iter_mut().zip(out) {
            p.extend(o);
        }
    };
    let order = match ext {
        "aac" => {
            let frames = flicks_aac::split_adts(data).unwrap();
            let mut dec = flicks_aac::Decoder::from_adts(&frames[0].0).unwrap();
            for (_, au) in &frames {
                append(dec.decode(au).unwrap());
            }
            layout::aac(dec.config().channel_config, dec.channels()).unwrap()
        }
        "ac3" | "eac3" => {
            let (mut dec, mut pos, mut order) = (flicks_ac3::Decoder::new(), 0, None);
            while pos < data.len() {
                let h = flicks_ac3::parse_header(&data[pos..]).unwrap();
                let out = dec.decode(&data[pos..]).unwrap();
                if !out.channels.is_empty() {
                    order = layout::ac3(h.acmod, h.lfeon);
                    append(out.channels);
                }
                pos += h.frame_bytes;
            }
            order.unwrap()
        }
        _ => {
            let (packets, granule) = ogg_packets(data);
            let head = flicks_opus::OpusHead::parse(&packets[0]).unwrap();
            let (family, pre_skip) = (head.mapping_family, head.pre_skip as usize);
            let mut dec = flicks_opus::Decoder::from_head(head, RATE as u32).unwrap();
            dec.set_trim_pre_skip(true);
            for p in &packets[2..] {
                append(dec.decode(Some(p)).unwrap());
            }
            let total = granule as usize - pre_skip;
            planar.iter_mut().for_each(|p| p.truncate(total));
            layout::opus(family, dec.channels()).unwrap()
        }
    };
    let mut out = Vec::new();
    interleave(&planar, &mut out);
    (out, order)
}

fn remix(src: &[Channel], out_channels: usize, input: &[f32]) -> Vec<f32> {
    let mut out = Vec::new();
    mix::apply(&mix::matrix(src, out_channels), src.len(), out_channels, input, &mut out);
    out
}

#[test]
fn surround_51_canonical_and_downmix() {
    let ff = flicks_testkit::require_ffmpeg!();
    let dir = flicks_testkit::fixtures_dir("audio");
    let n = 2 * RATE;
    let src: Vec<f32> = (0..n)
        .flat_map(|i| TONES.map(|f| AMP * (2.0 * std::f64::consts::PI * f * i as f64 / RATE as f64).sin() as f32))
        .collect();
    let raw = dir.join("tones51.f32");
    write_f32(&raw, &src);
    let raw = raw.to_str().unwrap();
    // AAC without PNS and Opus CELT-only, so our decoders match ffmpeg's to float precision
    // (PNS noise and the SILK resampler are decoder-specific); AC-3 differs by its dither.
    let mut cases: Vec<(&str, &[&str])> = vec![
        ("aac", &["-c:a", "aac", "-b:a", "384k", "-aac_pns", "0"]),
        ("ac3", &["-c:a", "ac3", "-b:a", "448k"]),
        ("eac3", &["-c:a", "eac3", "-b:a", "384k"]),
    ];
    if flicks_testkit::ffmpeg_has_encoder(&ff, "libopus") {
        cases.push(("opus", &["-c:a", "libopus", "-b:a", "256k", "-application", "lowdelay"]));
    } else {
        flicks_testkit::skip("audio downmix", "ffmpeg lacks libopus");
    }
    let g = std::f64::consts::FRAC_1_SQRT_2 * AMP as f64;
    // Expected stereo tone amplitudes: L, R, C (both at -3 dB), LFE (dropped), Ls, Rs (-3 dB).
    let lo = [AMP as f64, 0.0, g, 0.0, g, 0.0];
    let ro = [0.0, AMP as f64, g, 0.0, 0.0, g];
    println!(
        "{:<5} {:>14} {:>14} {:>12} {:>12} {:>9}",
        "codec", "5.1 SNR (min)", "stereo SNR", "C in Lo dB", "C in Ro dB", "leak dB"
    );
    for (ext, args) in cases {
        let coded = dir.join(format!("tones51.{ext}"));
        let ref6 = dir.join(format!("tones51.{ext}.ref6.f32"));
        let ref2 = dir.join(format!("tones51.{ext}.ref2.f32"));
        let ref6s = ref6.to_str().unwrap();
        let cs = coded.to_str().unwrap();
        ffmpeg(&ff, &[&["-f", "f32le", "-ar", "48000", "-ch_layout", "5.1", "-i", raw], args, &[cs]].concat());
        // ffmpeg's own decode in its 5.1 order, then libswresample's downmix of that decode.
        ffmpeg(&ff, &["-i", cs, "-f", "f32le", ref6s]);
        ffmpeg(
            &ff,
            &[
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ch_layout",
                "5.1",
                "-i",
                ref6s,
                "-ac",
                "2",
                "-f",
                "f32le",
                ref2.to_str().unwrap(),
            ],
        );
        let (ref6, ref2) = (read_f32(&ref6), read_f32(&ref2));
        let (ours, order) = decode(ext, &std::fs::read(&coded).unwrap());
        assert_eq!(order.len(), 6, "{ext}");

        let canonical = remix(order, 6, &ours);
        let stereo = remix(order, 2, &ours);
        let frames = canonical.len() / 6;
        assert!(frames.abs_diff(ref6.len() / 6) <= 1536, "{ext}: {frames} frames vs ffmpeg {}", ref6.len() / 6);
        let mut snr6 = f64::INFINITY;
        for c in 0..6 {
            let pick = |v: &[f32]| v.as_chunks::<6>().0.iter().map(|f| f[c]).collect::<Vec<_>>();
            snr6 = snr6.min(snr_db(&pick(&ref6), &pick(&canonical)));
            // Each canonical slot carries its own tone and (almost) nothing of the others.
            let own = tone(&canonical, 6, c, TONES[c]);
            assert!((own / AMP as f64 - 1.0).abs() < 0.05, "{ext}: slot {c} tone amplitude {own}");
        }
        let snr2 = snr_db(&ref2, &stereo);
        let mut leak = 0f64;
        for (t, &f) in TONES.iter().enumerate() {
            for (ch, want) in [(0, lo[t]), (1, ro[t])] {
                let got = tone(&stereo, 2, ch, f);
                if want == 0.0 {
                    leak = leak.max(got / AMP as f64);
                } else {
                    assert!(
                        (20.0 * (got / want).log10()).abs() < 0.3,
                        "{ext}: tone {f} Hz in ch {ch}: {got} want {want}"
                    );
                }
            }
        }
        let c_lo = 20.0 * (tone(&stereo, 2, 0, TONES[2]) / AMP as f64).log10();
        let c_ro = 20.0 * (tone(&stereo, 2, 1, TONES[2]) / AMP as f64).log10();
        let leak = 20.0 * leak.max(1e-12).log10();
        println!("{ext:<5} {snr6:>14.1} {snr2:>14.1} {c_lo:>12.2} {c_ro:>12.2} {leak:>9.1}");
        assert!(snr6 > 50.0 && snr2 > 50.0, "{ext}: SNR against ffmpeg 5.1 {snr6:.1} dB, stereo {snr2:.1} dB");
        assert!(leak < -50.0, "{ext}: a tone leaked into the wrong stereo channel at {leak:.1} dB");
        for p in [&coded, &dir.join(format!("tones51.{ext}.ref6.f32")), &dir.join(format!("tones51.{ext}.ref2.f32"))] {
            let _ = std::fs::remove_file(p);
        }
    }
    let _ = std::fs::remove_file(raw);
}

/// Fewer channels than the device: each speaker lands on its own canonical slot, never on the
/// next free one (4.0 surrounds must not reach C and LFE); a speaker the device lacks downmixes.
#[test]
fn pass_through_by_speaker() {
    use Channel::*;
    // Value k + 1 in source channel k.
    let frame = |n: usize| (1..=n).map(|v| v as f32).collect::<Vec<_>>();
    let cases: [(&[Channel], usize, &[f32]); 5] = [
        (&[L, R, Ls, Rs], 6, &[1.0, 2.0, 0.0, 0.0, 3.0, 4.0]),
        (&[L, C, R, Ls, Rs], 6, &[1.0, 3.0, 2.0, 0.0, 4.0, 5.0]),
        (&[C, L, R, Ls, Rs], 8, &[2.0, 3.0, 1.0, 0.0, 4.0, 5.0, 0.0, 0.0]),
        (layout::aac(6, 6).unwrap(), 6, &[2.0, 3.0, 1.0, 6.0, 4.0, 5.0]),
        // Quad on a four-channel device has no Ls/Rs slots: stereo downmix in the first two.
        (
            &[L, R, Ls, Rs],
            4,
            &[1.0 + 3.0 * std::f32::consts::FRAC_1_SQRT_2, 2.0 + 4.0 * std::f32::consts::FRAC_1_SQRT_2, 0.0, 0.0],
        ),
    ];
    for (src, device, want) in cases {
        let out = remix(src, device, &frame(src.len()));
        assert_eq!(out, want, "{src:?} on {device} channels");
    }
}
