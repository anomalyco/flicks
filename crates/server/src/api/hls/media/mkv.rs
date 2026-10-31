//! Indexed CFR Matroska. Do not scan or retain the rest of a hostile file for one cut.
use super::{MAX_SCAN_BYTES, Packet, Resource, TS, Track, Work, codec_name, invalid, sps_info};
use flicks_container::{
    isobmff::{self, CodecConfig, SampleEntry, TrackConfig, cmaf},
    matroska::{self, ByteSource, Codec, Demuxer},
};
use std::{cell::Cell, fs::File, io};

struct Input {
    file: File,
    len: u64,
    left: Cell<usize>,
    work: Work,
}
impl ByteSource for Input {
    fn len(&self) -> u64 {
        self.len
    }
    fn max_read_len(&self) -> u64 {
        16 << 20
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.left.set(self.left.get().checked_sub(buf.len()).ok_or_else(|| invalid("HLS read limit"))?);
        self.work.check()?;
        self.file.read_at(offset, buf)
    }
}

pub(super) fn config(
    t: &matroska::Track,
    index: usize,
    packet: Option<&matroska::Packet>,
) -> io::Result<Option<Track>> {
    if !t.enabled || !t.frames_readable() {
        return Ok(None);
    }
    let video = t.video.is_some();
    let mut partial_color = None;
    let (entry, duration) = match &t.codec {
        Codec::Avc { avcc } | Codec::Hevc { hvcc: avcc } => {
            let v = t.video.as_ref().ok_or_else(|| invalid("missing video"))?;
            if v.flag_interlaced == 1 {
                return Err(invalid("interlaced video"));
            }
            let w = u16::try_from(v.pixel_width).map_err(invalid)?;
            let h = u16::try_from(v.pixel_height).map_err(invalid)?;
            let duration = t
                .default_duration_ns
                .filter(|n| *n >= 1_000_000 && *n <= 1_000_000_000)
                .ok_or_else(|| invalid("CFR DefaultDuration required for PTS-only video"))?;
            let mut entry = if matches!(t.codec, Codec::Avc { .. }) {
                SampleEntry::avc(isobmff::AvcConfig::parse(avcc).map_err(invalid)?, w, h)
            } else {
                SampleEntry::hevc(isobmff::HevcConfig::parse(avcc).map_err(invalid)?, w, h)
            };
            if let Some(params) = &mut entry.video {
                let aspect = v.pixel_aspect();
                if aspect != (1, 1) {
                    params.pixel_aspect = Some(aspect);
                }
                if let Some(c) = &v.color {
                    let code = |v: Option<u64>| v.and_then(|v| u16::try_from(v).ok()).unwrap_or(2);
                    let (primaries, transfer, matrix) =
                        (code(c.primaries), code(c.transfer_characteristics), code(c.matrix_coefficients));
                    let color = match c.range {
                        Some(1 | 2) => {
                            isobmff::ColorInfo::Nclx { primaries, transfer, matrix, full_range: c.full_range() }
                        }
                        _ => isobmff::ColorInfo::Nclc { primaries, transfer, matrix },
                    };
                    if matches!(color, isobmff::ColorInfo::Nclx { .. }) && ![primaries, transfer, matrix].contains(&2) {
                        params.color = Some(color);
                    } else {
                        partial_color = Some(color);
                    }
                }
            }
            (entry, (duration / 1000) as u32)
        }
        Codec::Aac { asc } => {
            let (object, rate, _) = isobmff::parse_asc(asc).ok_or_else(|| invalid("AAC configuration"))?;
            // HE-AAC/960-sample frames require different packet durations.
            if object != 2 || rate == 0 || asc.get(1).is_none_or(|b| b & 4 != 0) {
                return Ok(None);
            }
            let a = t.audio.as_ref().ok_or_else(|| invalid("missing audio"))?;
            (SampleEntry::aac(asc.clone(), u32::try_from(a.channels).map_err(invalid)?, rate), 1024)
        }
        Codec::Ac3 | Codec::Eac3 => {
            let p = packet.ok_or_else(|| invalid("missing Dolby syncframe"))?;
            let enhanced = matches!(t.codec, Codec::Eac3);
            let (c, rate, samples) = cmaf::dolby_config(&p.data, enhanced).map_err(invalid)?;
            let a = t.audio.as_ref().ok_or_else(|| invalid("missing audio"))?;
            let format = if enhanced { *b"ec-3" } else { *b"ac-3" };
            (
                SampleEntry::audio(
                    (&format).into(),
                    c,
                    u32::try_from(a.channels).map_err(invalid)?,
                    f64::from(rate),
                    16,
                ),
                samples,
            )
        }
        _ => return Ok(None),
    };
    let timescale = entry.audio.as_ref().map_or(TS, |a| a.sample_rate as u32);
    let mut config = TrackConfig::new(entry, timescale);
    config.language = t.language.clone();
    cmaf::validate(&config).map_err(invalid)?;
    let codec = codec_name(&config.entry.codec)?;
    let (depth, sps_transfer) = sps_info(&config.entry.codec)?;
    Ok(Some(Track { index, config, duration, video, depth, sps_transfer, codec, partial_color }))
}

pub(super) struct Source {
    demux: Demuxer<Input>,
    video: usize,
}
impl Source {
    pub(super) fn open(file: File, work: Work) -> io::Result<Self> {
        let len = file.metadata()?.len();
        let demux = Demuxer::new(Input { file, len, left: Cell::new(MAX_SCAN_BYTES), work }).map_err(invalid)?;
        if demux.file().cues.is_empty() || demux.tracks().len() > 32 {
            return Err(invalid("indexed MKV with <=32 tracks required"));
        }
        let video =
            demux.tracks().iter().position(|t| t.enabled && t.video.is_some()).ok_or_else(|| invalid("no video"))?;
        Ok(Self { demux, video })
    }

    fn seek(&mut self, ms: u64, audio: bool) -> io::Result<()> {
        if ms == 0 {
            self.demux.rewind();
            return Ok(());
        }
        // Catalog cuts are floored to ms. Include the remainder of that ms for
        // video, but start at a prior cue for audio interleaved before the video block.
        let ns = if audio { ms * 1_000_000 - 1 } else { ms * 1_000_000 + 999_999 };
        self.demux
            .seek_cue(self.video, ns as i64, cmaf::MAX_SAMPLES)
            .map_err(invalid)?
            .ok_or_else(|| invalid("unusable video cue"))?;
        Ok(())
    }

    pub(super) fn tracks(&mut self, resource: Resource, start: u64) -> io::Result<Vec<Track>> {
        let selected = resource.rendition().map(|r| r.source_index(self.video));
        let video = self.video;
        let wanted = |i: usize| i == video || selected.is_none_or(|sel| i == sel);
        let dolby = self
            .demux
            .tracks()
            .iter()
            .enumerate()
            .filter(|(i, t)| wanted(*i) && t.enabled && matches!(t.codec, Codec::Ac3 | Codec::Eac3))
            .count();
        let mut first = std::collections::BTreeMap::new();
        if dolby > 0 {
            self.seek(start, true)?;
            for _ in 0..4096 {
                self.demux.source().work.check()?;
                let Some(p) = self.demux.next_packet().map_err(invalid)? else { break };
                if wanted(p.track)
                    && self.demux.tracks().get(p.track).is_some_and(|t| matches!(t.codec, Codec::Ac3 | Codec::Eac3))
                {
                    first.entry(p.track).or_insert(p);
                }
                if first.len() == dolby {
                    break;
                }
            }
        }
        let mut tracks = Vec::new();
        for (i, t) in self.demux.tracks().iter().enumerate() {
            if wanted(i)
                && let Some(track) = config(t, i, first.get(&i))?
            {
                tracks.push(track);
            }
        }
        Ok(tracks)
    }

    pub(super) fn packets(
        &mut self,
        track: &Track,
        start_ms: u64,
        end_ms: u64,
        transcode: bool,
    ) -> io::Result<Vec<Packet>> {
        let pad = if transcode && !track.video { 200 } else { 0 };
        let lo = (start_ms as i64 - pad) * 1000;
        let hi = (end_ms as i64 + pad) * 1000;
        self.seek(start_ms.saturating_sub(pad as u64), !track.video)?;
        let mut packets = Vec::new();
        let mut retained = 0usize;
        let mut scanned = 0usize;
        let mut begun = !track.video;
        let mut finished = false;
        let mut next_key = false;
        let mut trailing = 0;
        let mut keep = 0;
        for _ in 0..cmaf::MAX_SAMPLES {
            self.demux.source().work.check()?;
            let Some(mut p) = self.demux.next_packet().map_err(invalid)? else {
                finished = true;
                break;
            };
            scanned = scanned
                .checked_add(p.data.len())
                .filter(|n| *n <= MAX_SCAN_BYTES)
                .ok_or_else(|| invalid("segment scan limit"))?;
            if p.track != track.index {
                continue;
            }
            let pts = p.pts_ns.div_euclid(1000);
            if track.video {
                if p.keyframe && pts >= hi {
                    if !transcode {
                        finished = true;
                        break;
                    }
                    next_key = true;
                }
                // A CRA/recovery picture may be followed by leading pictures that
                // present in *this* cut. Decode through those pictures, but never
                // encode a picture presenting in the next cut (or seek back a GOP).
                if next_key && pts >= hi {
                    // Reorder depth bounds the *trailing* pictures decoded ahead
                    // of a leading picture, not the number of leading pictures.
                    trailing += 1;
                    if trailing > track.depth {
                        finished = true;
                        break;
                    }
                }
                if !begun {
                    if !p.keyframe || pts < lo {
                        continue;
                    }
                    if start_ms > 0 && pts.div_euclid(1000) != start_ms as i64 {
                        return Err(invalid("keyframe index changed"));
                    }
                    begun = true;
                }
            } else {
                if pts >= hi {
                    finished = true;
                    break;
                }
                if pts < lo {
                    continue;
                }
            }
            retained = retained
                .checked_add(p.data.len())
                .filter(|n| *n <= cmaf::MAX_SEGMENT_BYTES)
                .ok_or_else(|| invalid("segment size limit"))?;
            if let CodecConfig::Hevc(c) = &track.config.entry.codec {
                p.data = cmaf::hvc1_sample(c, &p.data).map_err(invalid)?;
            }
            packets.push(Packet {
                data: p.data,
                pts_us: pts,
                keyframe: p.keyframe,
                duration: track.duration,
                dts: None,
                pts: None,
            });
            if !next_key || pts < hi {
                keep = packets.len();
            }
        }
        if !finished {
            return Err(invalid("segment packet limit"));
        }
        packets.truncate(keep);
        Ok(packets)
    }
}
