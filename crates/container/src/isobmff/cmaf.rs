//! Stateless, bounded fragmented MP4 for HLS (ISO/IEC 14496-12:2022 and
//! 14496-15:2022). No file I/O; the same functions work in a wasm host.

use super::bytes::BoxBuf;
use super::{Brand, CodecConfig, Error, FragmentedWriter, Result, TrackConfig, WriteSample, WriterOptions};

pub mod timing;

/// Build the Dolby sample description from an independent syncframe (ETSI
/// TS 102 366 V1.4.1, Annex F). Dependent E-AC-3 substreams need aggregation and
/// are deliberately refused, rather than advertised as playable.
pub fn dolby_config(frame: &[u8], enhanced: bool) -> Result<(CodecConfig, u32, u32)> {
    use crate::bitstream::{BitReader, BitWriter};
    let bad = || Error::Invalid("Dolby syncframe".into());
    let mut r = BitReader::new(frame);
    let mut bits = |n| r.read_bits(n).map_err(|_| bad());
    if bits(16)? != 0x0b77 {
        return Err(bad());
    }
    let mut b = BitWriter::new();
    if enhanced {
        if bits(2)? != 0 || bits(3)? != 0 {
            return Err(Error::Unsupported("dependent E-AC-3".into()));
        }
        let bytes = (bits(11)? + 1) * 2;
        if bytes as usize != frame.len() {
            return Err(bad());
        }
        let fscod = bits(2)?;
        let (rate, blocks) = if fscod == 3 {
            let rate = [24000, 22050, 16000].get(bits(2)? as usize).copied().ok_or_else(bad)?;
            (rate, 6)
        } else {
            let blocks = [1, 2, 3, 6].get(bits(2)? as usize).copied().ok_or_else(bad)?;
            ([48000, 44100, 32000].get(fscod as usize).copied().ok_or_else(bad)?, blocks)
        };
        let acmod = bits(3)?;
        let lfe = bits(1)?;
        let bsid = bits(5)?;
        if !(11..=16).contains(&bsid) {
            return Err(bad());
        }
        let samples = blocks * 256;
        let kbps = (u64::from(bytes) * 8 * u64::from(rate)).div_ceil(u64::from(samples) * 1000);
        if kbps > 8191 {
            return Err(bad());
        }
        b.write_bits(kbps as u32, 13);
        b.write_bits(0, 3);
        b.write_bits(fscod, 2);
        b.write_bits(bsid, 5);
        b.write_bits(0, 2);
        b.write_bits(0, 3);
        b.write_bits(acmod, 3);
        b.write_bits(lfe, 1);
        b.write_bits(0, 8);
        Ok((CodecConfig::Eac3 { dec3: b.bytes().to_vec() }, rate, samples))
    } else {
        bits(16)?;
        let fscod = bits(2)?;
        let sizecode = bits(6)?;
        let bsid = bits(5)?;
        let bsmod = bits(3)?;
        let acmod = bits(3)?;
        if bsid > 8 || sizecode > 37 {
            return Err(bad());
        }
        if acmod & 1 != 0 && acmod != 1 {
            bits(2)?;
        }
        if acmod & 4 != 0 {
            bits(2)?;
        }
        if acmod == 2 {
            bits(2)?;
        }
        let lfe = bits(1)?;
        let rate = [48000, 44100, 32000].get(fscod as usize).copied().ok_or_else(bad)?;
        b.write_bits(fscod, 2);
        b.write_bits(bsid, 5);
        b.write_bits(bsmod, 3);
        b.write_bits(acmod, 3);
        b.write_bits(lfe, 1);
        b.write_bits(sizecode / 2, 5);
        b.write_bits(0, 5);
        Ok((CodecConfig::Ac3 { dac3: b.bytes().to_vec() }, rate, 1536))
    }
}

pub const MAX_SEGMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_SAMPLES: usize = 65_536;

/// Track IDs are one-based positions in `tracks`. Separate HLS renditions normally
/// pass one track, so each rendition uses track ID 1.
pub fn write_init_segment(tracks: &[TrackConfig]) -> Result<Vec<u8>> {
    if tracks.is_empty() || tracks.len() > 32 {
        return Err(Error::TooLarge("CMAF tracks"));
    }
    let mut config_bytes = 0usize;
    for track in tracks {
        validate(track)?;
        let mut add = |len: usize| -> Result<()> {
            config_bytes = config_bytes
                .checked_add(len)
                .filter(|n| *n <= 1024 * 1024)
                .ok_or(Error::TooLarge("CMAF configuration"))?;
            Ok(())
        };
        match &track.entry.codec {
            CodecConfig::Avc(c) => {
                for n in c.sps.iter().chain(&c.pps) {
                    add(n.len())?;
                }
                add(c.ext.len())?;
            }
            CodecConfig::Hevc(c) => {
                for n in c.arrays.iter().flat_map(|a| &a.nalus) {
                    add(n.len())?;
                }
            }
            _ => {}
        }
    }
    let mut writer = FragmentedWriter::new(Vec::new(), WriterOptions::new(Brand::Cmaf));
    for track in tracks {
        let mut track = track.clone();
        // Parameter sets live in the sample description, never an hev1 entry.
        match &mut track.entry.codec {
            CodecConfig::Avc(_) => track.entry.format = b"avc1".into(),
            CodecConfig::Hevc(h) => {
                track.entry.format = b"hvc1".into();
                for a in &mut h.arrays {
                    a.completeness = true;
                }
            }
            _ => {}
        }
        writer.add_track(track)?;
    }
    writer.finish()
}

pub fn validate(t: &TrackConfig) -> Result<()> {
    let bad = || Error::Mux("invalid CMAF track".into());
    if t.timescale == 0
        || t.language.len() > 64
        || !t.edits.is_empty()
        || t.media_start.is_some()
        || t.handler_name.as_ref().is_some_and(|s| s.len() > 256)
    {
        return Err(bad());
    }
    let nals_ok = |nals: &[Vec<u8>]| {
        !nals.is_empty() && nals.len() <= 31 && nals.iter().all(|n| !n.is_empty() && n.len() <= 65_535)
    };
    let valid = match &t.entry.codec {
        CodecConfig::Avc(c) => {
            matches!(c.length_size, 1 | 2 | 4) && nals_ok(&c.sps) && nals_ok(&c.pps) && c.ext.len() <= 65_535
        }
        CodecConfig::Hevc(c) => {
            matches!(c.length_size, 1 | 2 | 4)
                && c.arrays.len() <= 32
                && c.arrays.iter().all(|a| nals_ok(&a.nalus))
                && [32, 33, 34].iter().all(|n| c.arrays.iter().any(|a| a.nal_type == *n))
        }
        CodecConfig::Aac(c) => {
            c.asc.len() <= 64
                && super::parse_asc(&c.asc).is_some_and(|(_, rate, _)| rate > 0)
                && t.entry.format == b"mp4a"
        }
        CodecConfig::Ac3 { dac3 } => dac3.len() == 3 && t.entry.format == b"ac-3",
        CodecConfig::Eac3 { dec3 } => (5..=64).contains(&dec3.len()) && t.entry.format == b"ec-3",
        _ => false,
    };
    if !valid {
        return Err(bad());
    }
    let visual = matches!(t.entry.codec, CodecConfig::Avc(_) | CodecConfig::Hevc(_));
    if visual != t.entry.video.is_some() || visual == t.entry.audio.is_some() {
        return Err(bad());
    }
    if let Some(v) = &t.entry.video {
        if v.width == 0
            || v.height == 0
            || v.compressor_name.len() > 255
            || matches!(&v.color, Some(super::ColorInfo::Icc { profile, .. }) if profile.len() > 65_536)
        {
            return Err(bad());
        }
    } else if let Some(a) = &t.entry.audio {
        if !a.sample_rate.is_finite()
            || !(1.0..=65535.0).contains(&a.sample_rate)
            || a.channels == 0
            || a.channels > 32
            || a.qt_version != 0
        {
            return Err(bad());
        }
    } else {
        return Err(bad());
    }
    Ok(())
}

/// hvc1 stores parameter sets only in hvcC. Strip identical repeats from a
/// length-prefixed AU, refusing configuration changes (which need a new init).
pub fn hvc1_sample(c: &super::HevcConfig, data: &[u8]) -> Result<Vec<u8>> {
    if !matches!(c.length_size, 1 | 2 | 4) || data.len() > MAX_SEGMENT_BYTES {
        return Err(Error::TooLarge("HEVC sample"));
    }
    let mut out = Vec::new();
    let mut pos = 0usize;
    for _ in 0..4096 {
        if pos == data.len() {
            return if out.is_empty() { Err(Error::Mux("empty HEVC AU".into())) } else { Ok(out) };
        }
        let prefix = data.get(pos..pos + usize::from(c.length_size)).ok_or(Error::Truncated("HEVC NAL length"))?;
        let len = prefix.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
        let start = pos + prefix.len();
        let end = start.checked_add(len).ok_or(Error::TooLarge("HEVC NAL"))?;
        let nal = data.get(start..end).ok_or(Error::Truncated("HEVC NAL"))?;
        if nal.len() < 2 || nal.first().is_none_or(|n| n & 0x80 != 0) || nal.get(1).is_none_or(|n| n & 7 == 0) {
            return Err(Error::Invalid("HEVC NAL header".into()));
        }
        let kind = nal.first().copied().unwrap_or(0) >> 1 & 63;
        if matches!(kind, 32..=34) {
            if !c.arrays.iter().any(|a| a.nal_type == kind && a.nalus.iter().any(|n| n == nal)) {
                return Err(Error::Unsupported("in-band HEVC configuration change".into()));
            }
        } else {
            out.extend_from_slice(prefix);
            out.extend_from_slice(nal);
        }
        pos = end;
    }
    Err(Error::TooLarge("HEVC NAL count"))
}

/// A single-track fragment, with explicit sequence, absolute decode time and
/// signed composition offsets. Bytes depend only on these inputs (no clock/state).
pub fn write_media_segment(seq: u32, base_decode_time: u64, samples: &[WriteSample<'_>]) -> Result<Vec<u8>> {
    if seq == 0 || samples.is_empty() || samples.len() > MAX_SAMPLES {
        return Err(Error::Mux("invalid CMAF sequence/sample count".into()));
    }
    let mut size = 0usize;
    let mut end = base_decode_time;
    for s in samples {
        size =
            size.checked_add(s.data.len()).filter(|n| *n <= MAX_SEGMENT_BYTES).ok_or(Error::TooLarge("CMAF media"))?;
        if s.data.is_empty() || s.duration == 0 {
            return Err(Error::Mux("empty CMAF sample".into()));
        }
        end = end.checked_add(u64::from(s.duration)).ok_or(Error::TooLarge("CMAF timestamp"))?;
        let pts = i128::from(end - u64::from(s.duration)) + i128::from(s.composition_offset);
        if pts < 0 || pts > i128::from(u64::MAX) {
            return Err(Error::Mux("invalid CMAF PTS".into()));
        }
    }
    let mut b = BoxBuf::new();
    let moof = b.start(b"moof");
    let h = b.start_full(b"mfhd", 0, 0);
    b.u32(seq);
    b.end(h);
    let traf = b.start(b"traf");
    let h = b.start_full(b"tfhd", 0, 0x20000);
    b.u32(1);
    b.end(h);
    let h = b.start_full(b"tfdt", 1, 0);
    b.u64(base_decode_time);
    b.end(h);
    let h = b.start_full(b"trun", 1, 0xf01);
    b.u32(samples.len() as u32);
    // Fixed headers total 88 bytes, each trun sample is 16 bytes; mdat header is 8.
    b.i32((96 + 16 * samples.len()) as i32);
    let first_pts = i128::from(base_decode_time) + i128::from(samples.first().map_or(0, |s| s.composition_offset));
    let mut dts = base_decode_time;
    for s in samples {
        b.u32(s.duration);
        b.u32(s.data.len() as u32);
        // Open-GOP leading pictures precede the recovery point in presentation,
        // not decode order. Mark them as potentially undecodable on random access
        // (ISO/IEC 14496-12 sample_is_leading=1); decoders suppress RASL on a seek.
        let leading = i128::from(dts) + i128::from(s.composition_offset) < first_pts;
        b.u32((if s.is_sync { 0x0200_0000 } else { 0x0101_0000 }) | if leading { 0x0400_0000 } else { 0 });
        b.i32(s.composition_offset);
        dts += u64::from(s.duration); // Overflow was checked above.
    }
    b.end(h);
    b.end(traf);
    b.end(moof);
    let h = b.start(b"mdat");
    for s in samples {
        b.bytes(s.data);
    }
    b.end(h);
    Ok(b.buf)
}
