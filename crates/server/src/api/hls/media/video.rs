use super::{Packet, TS, Tier, Track, invalid};
use flicks_container::isobmff::{AvcConfig, CodecConfig, ColorInfo, SampleEntry, TrackConfig, WriteSample, cmaf};
use flicks_platform::{Codec, Decoder, Frame, H264Encoder};
use std::{io, time::Instant};

fn format(track: &Track) -> io::Result<(Codec, Vec<u8>)> {
    Ok(match &track.config.entry.codec {
        CodecConfig::Avc(c) => (Codec::H264, c.to_bytes()),
        CodecConfig::Hevc(c) => (Codec::Hevc, c.to_bytes()),
        _ => return Err(invalid("unsupported video source")),
    })
}

pub(super) fn supported(track: &Track) -> bool {
    format(track).is_ok_and(|(c, p)| {
        Decoder::transcode_format(c, &p).is_ok_and(|f| {
            track
                .config
                .entry
                .video
                .as_ref()
                .is_some_and(|v| (u32::from(v.width), u32::from(v.height)) == (f.crop.2, f.crop.3))
        })
    })
}

/// Hardware-first decode, one pixel transfer into the encoder's IOSurface pool,
/// no CPU plane copies. Return both resources so /init needn't encode cut zero twice.
pub(super) fn segment(
    track: &Track,
    tier: Tier,
    packets: &[Packet],
    seq: u32,
    cut: std::ops::Range<u64>,
    delay_us: u64,
    deadline: Instant,
) -> io::Result<(Vec<u8>, Vec<u8>)> {
    if packets.len() > 2065 {
        return Err(invalid("video segment frame limit"));
    }
    let (codec, config) = format(track)?;
    let frame_us = u32::try_from(u64::from(track.duration) * u64::from(TS) / u64::from(track.config.timescale))
        .map_err(invalid)?;
    let mut decoder = Decoder::for_transcode(codec, &config, track.source_color()).map_err(invalid)?;
    let mut encoder = H264Encoder::new(tier.width, tier.height, tier.bitrate, frame_us).map_err(invalid)?;
    let range = (cut.start as i64 * 1000)..(cut.end as i64 * 1000);
    let mut source_pts = Vec::new();
    let mut encoder_pts: Option<i64> = None;
    let mut encode = |frame: Frame| -> io::Result<()> {
        if Instant::now() >= deadline {
            return Err(invalid("video encode deadline"));
        }
        if range.contains(&frame.pts) {
            // MP4 can timestamp two pictures at the same instant (the real episode
            // does). VT requires strictly increasing submission times. Separate its
            // decode ticks by 1 µs and restore the *exact* source PTS with trun ctts.
            let stamp = frame.pts.max(encoder_pts.map_or(frame.pts, |last| last.saturating_add(1)));
            encoder.encode(frame.pixel_buffer(), stamp, false).map_err(invalid)?;
            source_pts.push(frame.pts);
            encoder_pts = Some(stamp);
        }
        Ok(())
    };
    for p in packets {
        if Instant::now() >= deadline {
            return Err(invalid("video decode deadline"));
        }
        for f in decoder.decode(&p.data, p.pts_us).map_err(invalid)? {
            encode(f)?;
        }
    }
    for f in decoder.flush().map_err(invalid)? {
        encode(f)?;
    }
    let out = encoder.finish().map_err(invalid)?;
    let avcc = AvcConfig::parse(&out.avcc).map_err(invalid)?;
    let mut entry = SampleEntry::avc(avcc, tier.width, tier.height);
    if let Some(v) = &mut entry.video {
        v.color = Some(ColorInfo::Nclx { primaries: 1, transfer: 1, matrix: 1, full_range: false });
    }
    let init = cmaf::write_init_segment(&[TrackConfig::new(entry, TS)]).map_err(invalid)?;
    let first = out.samples.first().ok_or_else(|| invalid("empty encoded segment"))?;
    let base = u64::try_from(first.pts_us)
        .map_err(invalid)?
        .checked_add(delay_us)
        .ok_or_else(|| invalid("presentation time overflow"))?;
    let mut samples = Vec::with_capacity(out.samples.len());
    for (i, s) in out.samples.iter().enumerate() {
        let pts = *source_pts.get(i).ok_or_else(|| invalid("unexpected encoded picture"))?;
        let composition_offset = i32::try_from(i128::from(pts) - i128::from(s.pts_us)).map_err(invalid)?;
        // No B-frames in this rendition. Preserve each source presentation instant,
        // including container VFR and Matroska's rounded millisecond timestamps.
        let duration = if let Some(next) = out.samples.get(i + 1) {
            u32::try_from(i128::from(next.pts_us) - i128::from(s.pts_us)).map_err(invalid)?
        } else {
            let packet = packets
                .iter()
                .rev()
                .find(|p| p.pts_us == pts)
                .ok_or_else(|| invalid("unexpected encoder timestamp"))?;
            let nominal = u64::from(packet.duration) * u64::from(TS) / u64::from(track.config.timescale);
            u32::try_from((i128::from(nominal) + i128::from(composition_offset)).max(1)).map_err(invalid)?
        };
        samples.push(WriteSample { data: &s.data, duration, composition_offset, is_sync: s.idr });
    }
    Ok((init, cmaf::write_media_segment(seq, base, &samples).map_err(invalid)?))
}
