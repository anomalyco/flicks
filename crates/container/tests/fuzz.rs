//! Deterministic mutation fuzzing: the demuxer must never panic on corrupted input.

mod common;

use flicks_container::isobmff::*;
use std::io::Cursor;

struct Rng(u64);

#[test]
fn cmaf_writer_hostile_inputs() {
    use flicks_container::isobmff::cmaf;
    let mut rng = Rng(0x19ca_fef0_1234);
    for i in 0..10_000 {
        let len = rng.below(256);
        let bytes: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
        let mut cfg = TrackConfig::new(
            SampleEntry::aac(bytes.clone(), rng.next() as u32, f64::from_bits(rng.next()) as u32),
            rng.next() as u32,
        );
        if i % 2 == 0 {
            cfg.entry =
                SampleEntry::avc(AvcConfig::new(vec![bytes.clone()], vec![bytes.clone()], rng.next() as u8), 16, 16);
        }
        let _ = cmaf::write_init_segment(&[cfg.clone()]);
        let _ = cmaf::timing::reorder_depth(&cfg.entry.codec);
        if let Ok(c) = HevcConfig::parse(&bytes) {
            let _ = cmaf::timing::reorder_depth(&CodecConfig::Hevc(c));
        }
        let _ = cmaf::dolby_config(&bytes, false);
        let _ = cmaf::dolby_config(&bytes, true);
        let sample = WriteSample {
            data: &bytes,
            duration: rng.next() as u32,
            composition_offset: rng.next() as i32,
            is_sync: i % 2 == 0,
        };
        let base = rng.next();
        if let Ok(fragment) = cmaf::write_media_segment(1, base, &[sample]) {
            assert_eq!(fragment, cmaf::write_media_segment(1, base, &[sample]).unwrap());
            assert!(fragment.len() < bytes.len() + 256);
        }
        let _ = cmaf::timing::timestamp(
            rng.next() as i64,
            rng.next() as i64,
            rng.below(100_000),
            rng.next() as u32,
            rng.next() as u32,
        );
    }
    let sample = WriteSample { data: &[1], duration: 1, composition_offset: 0, is_sync: true };
    assert!(cmaf::write_media_segment(0, 0, &[sample]).is_err());
    assert!(cmaf::write_media_segment(1, u64::MAX, &[sample]).is_err());
    assert!(cmaf::write_media_segment(1, 0, &vec![sample; cmaf::MAX_SAMPLES + 1]).is_err());
    assert!(cmaf::write_init_segment(&[]).is_err());
    let cfg = TrackConfig::new(SampleEntry::aac(vec![0x11, 0x90], 2, 48000), 48000);
    let mut bytes = cmaf::write_init_segment(&[cfg]).unwrap();
    bytes.extend(cmaf::write_media_segment(77, 12345, &[sample]).unwrap());
    let parsed = open(bytes.as_slice()).unwrap();
    let s = parsed.tracks[0].samples.get(0).unwrap();
    assert_eq!(s.dts, 12345);
    assert_eq!(parsed.read_sample(bytes.as_slice(), 0, 0).unwrap(), [1]);
}
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

#[test]
fn cmaf_configuration_prefixes_and_bit_flips() {
    use flicks_container::isobmff::cmaf;
    let mut count = 0;
    for name in ["h264_bframes.mp4", "hevc.mp4"] {
        let Some(path) = common::fixture(name) else {
            return;
        };
        let source = std::fs::read(path).unwrap();
        let doc = open(source.as_slice()).unwrap();
        let track = doc.tracks.iter().find(|t| t.kind == TrackKind::Video).unwrap();
        let entry = track.entries[0].clone();
        assert!(cmaf::timing::reorder_depth(&entry.codec).unwrap() <= 16);
        let sps = match &entry.codec {
            CodecConfig::Avc(c) => c.sps[0].clone(),
            CodecConfig::Hevc(c) => c.sps()[0].to_vec(),
            _ => unreachable!(),
        };
        let mut cases: Vec<Vec<u8>> = (0..sps.len()).map(|n| sps[..n].to_vec()).collect();
        for i in 0..sps.len() {
            for bit in 0..8 {
                let mut bytes = sps.clone();
                bytes[i] ^= 1 << bit;
                cases.push(bytes);
            }
        }
        for bytes in cases {
            let mut entry = entry.clone();
            match &mut entry.codec {
                CodecConfig::Avc(c) => c.sps = vec![bytes],
                CodecConfig::Hevc(c) => c.arrays.iter_mut().find(|a| a.nal_type == 33).unwrap().nalus = vec![bytes],
                _ => unreachable!(),
            }
            if let Ok(depth) = cmaf::timing::reorder_depth(&entry.codec) {
                assert!(depth <= 16);
            }
            let _ = cmaf::write_init_segment(&[TrackConfig::new(entry, track.timescale)]);
            count += 1;
        }
        if let CodecConfig::Hevc(c) = &entry.codec {
            let sample = doc.read_sample(source.as_slice(), 0, 0).unwrap();
            for n in 0..sample.len() {
                let _ = cmaf::hvc1_sample(c, &sample[..n]);
                count += 1;
            }
            let mut repeated = Vec::new();
            for n in c.arrays.iter().filter(|a| matches!(a.nal_type, 32..=34)).flat_map(|a| &a.nalus) {
                repeated.extend((n.len() as u32).to_be_bytes());
                repeated.extend(n);
            }
            repeated.extend(&sample);
            assert_eq!(cmaf::hvc1_sample(c, &repeated).unwrap(), cmaf::hvc1_sample(c, &sample).unwrap());
        }
    }
    eprintln!("CMAF configuration/sample mutations: {count}");
}

fn synthetic_files() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for brand in [Brand::Mp4, Brand::Mov] {
        let mut w = Mp4Writer::new(Cursor::new(Vec::new()), WriterOptions::new(brand)).unwrap();
        let v = w
            .add_track(TrackConfig::new(
                SampleEntry::avc(AvcConfig::new(vec![vec![0x67, 0x64, 0, 0x1F]], vec![vec![0x68]], 4), 64, 64),
                25,
            ))
            .unwrap();
        let mut cfg = TrackConfig::new(SampleEntry::aac(vec![0x11, 0x90], 2, 48000), 48000);
        cfg.media_start = Some(1024);
        let a = w.add_track(cfg).unwrap();
        let p = PcmConfig { bits: 16, float: false, big_endian: false, signed: true, channels: 1, sample_rate: 8000.0 };
        let pc = w.add_track(TrackConfig::new(SampleEntry::pcm(p), 8000)).unwrap();
        w.add_timecode_track(
            v,
            TimecodeConfig {
                flags: 1,
                timescale: 30000,
                frame_duration: 1001,
                frames_per_second: 30,
                ..Default::default()
            },
            1000,
        )
        .unwrap();
        for i in 0..20u8 {
            w.write_sample(
                v,
                WriteSample { data: &[i; 20], duration: 1, composition_offset: (i % 3) as i32, is_sync: i % 10 == 0 },
            )
            .unwrap();
            w.write_sample(a, WriteSample { data: &[i; 7], duration: 1024, composition_offset: 0, is_sync: true })
                .unwrap();
            w.write_sample(pc, WriteSample { data: &[i; 16], duration: 0, composition_offset: 0, is_sync: true })
                .unwrap();
        }
        out.push(w.finish().unwrap().into_inner());
    }
    let mut w = FragmentedWriter::new(Vec::new(), WriterOptions::new(Brand::Mp4));
    let v = w.add_track(TrackConfig::new(SampleEntry::jpeg(16, 16), 1000)).unwrap();
    for i in 0..12u8 {
        w.write_sample(v, WriteSample { data: &[i; 9], duration: 40, composition_offset: -1, is_sync: i % 4 == 0 })
            .unwrap();
        if i % 4 == 3 {
            w.flush_fragment().unwrap();
        }
    }
    out.push(w.finish().unwrap());
    out
}

/// Walk every box as if it were a container, checking the iterator's offsets against `data`.
fn walk_boxes(data: &[u8], range: std::ops::Range<usize>, depth: usize, budget: &mut usize, lenient: bool) {
    let mut it = boxes::BoxIter::within(data, range.clone());
    if lenient {
        it = it.allow_truncated();
    }
    let mut next = range.start;
    for b in it.by_ref() {
        let Ok(b) = b else { break };
        assert!(b.offset == next && b.end() <= range.end && b.payload_range().start >= b.offset + 8);
        assert_eq!(data.get(b.payload_range()), Some(b.payload));
        next = b.end();
        if depth < 8 && *budget > 0 {
            *budget -= 1;
            walk_boxes(data, b.payload_range(), depth + 1, budget, lenient);
        }
    }
    assert!(it.next().is_none() && it.offset() <= range.end);
}

fn exercise(data: &[u8]) -> bool {
    walk_boxes(data, 0..data.len(), 0, &mut 2000, false);
    walk_boxes(data, 0..data.len(), 0, &mut 200, true);
    let r = open(data);
    let ok = r.is_ok();
    if let Ok(f) = r {
        for (ti, t) in f.tracks.iter().enumerate() {
            let n = t.samples.len();
            for i in [0, n / 2, n.saturating_sub(1)] {
                _ = f.read_sample(data, ti, i);
                _ = t.sync_sample_before(i);
                _ = t.presentation_pts(i);
            }
            _ = t.sample_at_pts(i64::MIN);
            _ = t.sample_at_pts(i64::MAX);
            _ = t.sample_at_presentation_time(0);
            if let Some(CodecConfig::Timecode(tc)) = t.codec() {
                _ = tc.format_frame(tc.start_frame.unwrap_or(u32::MAX));
            }
        }
    }
    ok
}

fn mutate_all(base: &[u8], seed: u64, iterations: usize) {
    let mut rng = Rng(seed | 1);
    // Focus mutations on the moov/moof boxes (where parsing happens).
    let hot: Vec<(usize, usize)> = {
        let mut v = Vec::new();
        for name in [b"moov", b"moof"] {
            let mut from = 0;
            while let Some(p) = base[from..].windows(4).position(|w| w == name) {
                let at = from + p;
                let start = at.saturating_sub(4);
                let size = u32::from_be_bytes(base[start..start + 4].try_into().unwrap()) as usize;
                v.push((start, (start + size.max(8)).min(base.len())));
                from = at + 4;
            }
        }
        if v.is_empty() {
            v.push((0, base.len()));
        }
        v
    };
    let mut parsed = 0usize;
    for _ in 0..iterations {
        let mut d = base.to_vec();
        let (lo, hi) = hot[rng.below(hot.len())];
        let edits = 1 + rng.below(4);
        for _ in 0..edits {
            let pos = if rng.below(10) < 8 { lo + rng.below(hi - lo) } else { rng.below(d.len()) };
            let pos = pos.min(d.len() - 1);
            match rng.below(6) {
                0 => d[pos] ^= 1 << rng.below(8),
                1 => d[pos] = rng.next() as u8,
                2 | 3 => {
                    // Overwrite a 32-bit field with an extreme value.
                    let v: u32 = match rng.below(5) {
                        0 => 0,
                        1 => 1,
                        2 => u32::MAX,
                        3 => 0x8000_0000,
                        _ => rng.next() as u32,
                    };
                    let end = (pos + 4).min(d.len());
                    d[pos..end].copy_from_slice(&v.to_be_bytes()[..end - pos]);
                }
                4 => d.truncate(pos.max(8)),
                _ => {
                    let n = rng.below(16).min(d.len() - pos);
                    d.drain(pos..pos + n);
                }
            }
            if d.is_empty() {
                break;
            }
        }
        parsed += exercise(&d) as usize;
    }
    eprintln!("seed {seed:#x}: {parsed}/{iterations} mutated files still parsed");
}

#[test]
fn mutated_synthetic_files_never_panic() {
    for (i, f) in synthetic_files().iter().enumerate() {
        exercise(f);
        mutate_all(f, 0x5EED_0000 + i as u64, 20_000);
    }
}

#[test]
fn mutated_ffmpeg_fixtures_never_panic() {
    for (i, name) in
        ["h264_bframes.mp4", "frag.mp4", "tmcd_2997df.mov", "prores.mov", "hevc.mp4", "aac.mp4"].iter().enumerate()
    {
        let Some(p) = common::fixture(name) else { continue };
        let data = std::fs::read(p).unwrap();
        mutate_all(&data, 0xF00D + i as u64, 5000);
    }
}

#[test]
fn truncations_never_panic() {
    for f in synthetic_files() {
        for n in 0..f.len().min(4096) {
            exercise(&f[..n]);
        }
    }
}
