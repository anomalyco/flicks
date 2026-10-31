//! Independent AAC-LC stereo segments on one absolute AAC sample grid.
#![cfg(target_os = "macos")]

use super::{Packet, Track, Work, invalid};
use flicks_audio::{Channel, interleave, layout, mix};
use flicks_container::isobmff::{CodecConfig, SampleEntry, TrackConfig, WriteSample, cmaf};
use flicks_platform::AacEncoder;
use std::io;

const FRAME: u64 = 1024;
const PREROLL: usize = 4096;
const MAX_PACKETS: usize = 8192;

fn output_config(track: &Track) -> io::Result<TrackConfig> {
    let rate = match &track.config.entry.codec {
        CodecConfig::Aac(c) => {
            if c.asc.len() > 64 {
                return Err(invalid("AAC configuration size"));
            }
            let config = flicks_aac::AudioSpecificConfig::parse(&c.asc).map_err(invalid)?;
            if config.object_type != 2 || config.sbr || config.ps || config.frame_length_flag {
                return Err(invalid("stereo rendition requires AAC-LC"));
            }
            config.sample_rate
        }
        CodecConfig::Ac3 { .. } | CodecConfig::Eac3 { .. } => {
            let rate = track.config.entry.audio.as_ref().ok_or_else(|| invalid("missing audio format"))?.sample_rate;
            if !rate.is_finite() || rate.fract() != 0.0 || !(8000.0..=48000.0).contains(&rate) {
                return Err(invalid("stereo sample rate"));
            }
            rate as u32
        }
        _ => return Err(invalid("unsupported stereo source")),
    };
    let index = flicks_aac::SAMPLE_RATES
        .iter()
        .position(|r| *r == rate)
        .filter(|_| (8000..=48000).contains(&rate))
        .ok_or_else(|| invalid("stereo sample rate"))?;
    let asc = flicks_aac::AudioSpecificConfig::lc(index as u8, 2).to_bytes();
    let mut config = TrackConfig::new(SampleEntry::aac(asc, 2, rate), rate);
    config.language = track.config.language.clone();
    Ok(config)
}

pub(super) fn supported(track: &Track) -> bool {
    output_config(track).is_ok()
}

pub(super) fn init(track: &Track) -> io::Result<Vec<u8>> {
    cmaf::write_init_segment(&[output_config(track)?]).map_err(invalid)
}

// Ceiling on the absolute grid, not a rounded segment duration: the preceding
// end is exactly the following start even at 44.1 kHz and nonintegral cuts.
fn boundary(ms: u64, rate: u32) -> io::Result<u64> {
    let frames = u128::from(ms) * u128::from(rate);
    u64::try_from(frames.div_ceil(1000 * u128::from(FRAME)) * u128::from(FRAME)).map_err(invalid)
}

pub(super) fn segment(
    track: &Track,
    packets: &[Packet],
    seq: usize,
    start_ms: u64,
    end_ms: u64,
    delay_us: u64,
    work: &Work,
) -> io::Result<Vec<u8>> {
    if end_ms <= start_ms || end_ms - start_ms > 32_000 || packets.len() > MAX_PACKETS {
        return Err(invalid("stereo segment duration or packet count"));
    }
    let mut bytes = 0usize;
    for packet in packets {
        bytes = bytes
            .checked_add(packet.data.len())
            .filter(|n| *n <= cmaf::MAX_SEGMENT_BYTES)
            .ok_or_else(|| invalid("stereo packet bytes"))?;
        if packet.data.is_empty() || packet.data.len() > 1024 * 1024 {
            return Err(invalid("stereo packet size"));
        }
    }
    let rate = output_config(track)?.timescale;
    let start = boundary(start_ms, rate)?;
    let end = boundary(end_ms, rate)?;
    let frames = usize::try_from(end - start).map_err(invalid)?;
    if frames == 0 {
        return Err(invalid("stereo segment shorter than one AAC frame"));
    }
    let encoder = AacEncoder::new(rate).map_err(invalid)?;
    let priming = encoder.priming_frames() as usize;
    // Apple's encoder has a non-packet-aligned delay (normally 2112). Padding
    // *before* the real preroll makes delay + padding + preroll whole packets.
    // Discard those packets rather than adding an edit list or silence per cut.
    let padding = (FRAME as usize - priming % FRAME as usize) % FRAME as usize;
    let skip = (priming + padding + PREROLL) / FRAME as usize;
    // Source audio may end before video. An empty cut is still the same AAC timeline:
    // encode silence on its absolute grid instead of turning the advertised URI into a 422.
    let mut pcm = vec![0.0; (padding + PREROLL + frames + FRAME as usize) * 2];
    let origin = i128::from(start) - PREROLL as i128;
    let mut aac = match &track.config.entry.codec {
        CodecConfig::Aac(c) => Some(flicks_aac::Decoder::new(&c.asc).map_err(invalid)?),
        _ => None,
    };
    let mut dolby = flicks_ac3::Decoder::new();
    dolby.set_drc(false);
    let mut decoded_frames = 0usize;
    let mut write = |channels: &[Vec<f32>], speakers: &[Channel], sample: i128| -> io::Result<()> {
        let count = channels.first().map_or(0, Vec::len);
        if count == 0
            || channels.len() != speakers.len()
            || channels.len() > 8
            || channels.iter().any(|c| c.len() != count)
        {
            return Err(invalid("decoded audio layout"));
        }
        decoded_frames = decoded_frames
            .checked_add(count)
            .filter(|n| *n <= rate as usize * 33)
            .ok_or_else(|| invalid("stereo decode duration limit"))?;
        let mut input = Vec::new();
        interleave(channels, &mut input);
        let mut stereo = Vec::new();
        mix::apply(&mix::matrix(speakers, 2), speakers.len(), 2, &input, &mut stereo);
        for (i, frame) in stereo.as_chunks::<2>().0.iter().enumerate() {
            let position = sample + i as i128 - origin;
            if position < 0 || position >= (PREROLL + frames + FRAME as usize) as i128 {
                continue;
            }
            let at = (padding + position as usize) * 2;
            let target = pcm.get_mut(at..at + 2).ok_or_else(|| invalid("stereo PCM bounds"))?;
            for (dst, src) in target.iter_mut().zip(frame) {
                if !src.is_finite() {
                    return Err(invalid("nonfinite decoded audio"));
                }
                *dst = src.clamp(-1.0, 1.0);
            }
        }
        Ok(())
    };
    let mut next_sample: Option<i128> = None;
    for packet in packets {
        work.check()?;
        let timestamp = i128::from(packet.pts_us) * i128::from(rate);
        // Round to the closest source sample, including negative preroll times.
        let nominal = (timestamp + 500_000).div_euclid(1_000_000);
        // Matroska commonly rounds packet timestamps to whole milliseconds. Don't
        // insert/drop 16 PCM samples in every 1024-sample AAC block because of that.
        let mut sample =
            next_sample.filter(|s| (*s - nominal).abs() <= i128::from(rate.div_ceil(1000))).unwrap_or(nominal);
        if let Some(decoder) = aac.as_mut() {
            if packet.data.len() > 65_536 {
                return Err(invalid("AAC packet size"));
            }
            let channels = decoder.decode(&packet.data).map_err(invalid)?;
            let speakers = layout::aac(decoder.config().channel_config, channels.len())
                .ok_or_else(|| invalid("AAC channel layout"))?;
            write(&channels, speakers, sample)?;
            sample += i128::from(FRAME);
        } else {
            let mut data = packet.data.as_slice();
            while !data.is_empty() {
                work.check()?;
                // Account for every aggregated E-AC-3 frame in write's decoded
                // sample budget; the outer packet cap alone doesn't bound them.
                let header = flicks_ac3::parse_header(data).map_err(invalid)?;
                if !header.is_primary() || header.sample_rate != rate || header.frame_bytes == 0 {
                    return Err(invalid("Dolby substream or sample rate change"));
                }
                let frame = data.get(..header.frame_bytes).ok_or_else(|| invalid("truncated Dolby frame"))?;
                let decoded = dolby.decode(frame).map_err(invalid)?;
                let speakers =
                    layout::ac3(header.acmod, header.lfeon).ok_or_else(|| invalid("Dolby channel layout"))?;
                write(&decoded.channels, speakers, sample)?;
                sample += header.samples() as i128;
                data = data.get(header.frame_bytes..).ok_or_else(|| invalid("Dolby frame size"))?;
            }
        }
        next_sample = Some(sample);
    }
    work.check()?;
    let encoded = encoder.encode(&pcm).map_err(invalid)?;
    work.check()?;
    let count = frames / FRAME as usize;
    let selected = encoded.get(skip..skip + count).ok_or_else(|| invalid("short AAC encoder output"))?;
    let samples: Vec<_> = selected
        .iter()
        .map(|data| WriteSample { data, duration: FRAME as u32, composition_offset: 0, is_sync: true })
        .collect();
    let sequence = u32::try_from(seq).map_err(invalid)?.checked_add(1).ok_or_else(|| invalid("segment sequence"))?;
    // The video rendition shifts its DTS to accommodate B-frame reordering;
    // apply the same constant presentation delay, not a per-segment rounding.
    let delay = u64::try_from((u128::from(delay_us) * u128::from(rate) + 500_000) / 1_000_000).map_err(invalid)?;
    let base = start.checked_add(delay).ok_or_else(|| invalid("stereo timestamp overflow"))?;
    cmaf::write_media_segment(sequence, base, &samples).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cut_past_the_audio_tail_decodes_to_silence_on_the_same_grid() {
        let config = TrackConfig::new(SampleEntry::aac(vec![0x11, 0x90], 2, 48000), 48000);
        let track = Track {
            index: 1,
            config,
            duration: 1024,
            video: false,
            depth: 0,
            sps_transfer: 2,
            codec: "mp4a.40.2".into(),
            partial_color: None,
        };
        let data = segment(
            &track,
            &[],
            1,
            4000,
            8000,
            0,
            &Work::new(std::time::Instant::now() + std::time::Duration::from_secs(5)),
        )
        .unwrap();
        let samples = cmaf::read_media_segment(&data, 2).unwrap();
        assert_eq!(samples[0].dts, boundary(4000, 48000).unwrap() as i64);
        let mut d = flicks_aac::Decoder::new(&[0x11, 0x90]).unwrap();
        for s in &samples {
            let pcm = d.decode(&data[s.offset as usize..(s.offset + u64::from(s.size)) as usize]).unwrap();
            assert!(pcm.iter().flatten().all(|v| v.abs() < 0.0001));
        }
        assert_eq!(samples.last().unwrap().dts + 1024, boundary(8000, 48000).unwrap() as i64);
    }

    #[test]
    fn cuts_share_one_absolute_aac_grid() {
        for rate in [8000, 44100, 48000] {
            let mut previous = 0;
            for ms in [0, 1001, 2002, 3003, 4004, 5005] {
                let cut = boundary(ms, rate).unwrap();
                assert_eq!(cut % FRAME, 0);
                assert!(cut >= ms * u64::from(rate) / 1000);
                assert!(cut - ms * u64::from(rate) / 1000 < FRAME);
                assert_eq!((cut - previous) % FRAME, 0);
                previous = cut;
            }
        }
        assert!(boundary(u64::MAX, 48000).is_err());
    }
}
