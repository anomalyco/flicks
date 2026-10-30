use crate::{Error, Result, Source, Track, TrackKind, source::Bytes};
use flicks_container::{isobmff as mp4, matroska as mkv};
use mkv::ByteSource;

pub(crate) struct Packet {
    pub track: usize,
    pub pts: i64, // nanoseconds, including container edits / codec delay
    pub duration: u64,
    pub discard_padding: i64,
    pub data: Vec<u8>,
}

pub(crate) enum Demux {
    Mkv(Box<mkv::Demuxer<Bytes>>),
    Mp4 { file: Box<mp4::Mp4File>, bytes: Bytes, cursors: Vec<usize> },
}

impl Demux {
    pub fn open(source: Source) -> Result<(Self, Vec<Track>, f64)> {
        let bytes = Bytes::open(source)?;
        let mut magic = [0; 4];
        bytes.read_at(0, &mut magic)?;
        if magic == [0x1a, 0x45, 0xdf, 0xa3] {
            let demux = mkv::Demuxer::new(bytes)?;
            let duration = demux.duration_ns().unwrap_or(0) as f64 / 1e9;
            let tracks = demux
                .tracks()
                .iter()
                .enumerate()
                .map(|(id, t)| Track {
                    id,
                    kind: match t.kind {
                        mkv::TrackKind::Video => TrackKind::Video,
                        mkv::TrackKind::Audio => TrackKind::Audio,
                        mkv::TrackKind::Subtitle => TrackKind::Subtitle,
                        _ => TrackKind::Other,
                    },
                    codec: t.codec.clone(),
                    language: t.language.clone(),
                    name: t.name.clone(),
                    default: t.default,
                    enabled: t.enabled && t.frames_readable(),
                })
                .collect();
            Ok((Self::Mkv(Box::new(demux)), tracks, duration))
        } else {
            let file = mp4::open(&bytes)?;
            let duration = if file.timescale == 0 { 0.0 } else { file.duration as f64 / file.timescale as f64 };
            let tracks = file
                .tracks
                .iter()
                .enumerate()
                .map(|(id, t)| {
                    use mkv::Codec as M;
                    use mp4::CodecConfig as C;
                    let codec = match t.codec() {
                        Some(C::Avc(c)) => M::Avc { avcc: c.to_bytes() },
                        Some(C::Hevc(c)) => M::Hevc { hvcc: c.to_bytes() },
                        Some(C::Av1(c)) => M::Av1 { av1c: c.to_bytes() },
                        Some(C::Vp9(_)) => M::Vp9 { private: Vec::new() },
                        Some(C::Aac(c)) => M::Aac { asc: c.asc.clone() },
                        Some(C::Ac3 { .. }) => M::Ac3,
                        Some(C::Eac3 { .. }) => M::Eac3,
                        Some(C::Opus(c)) => M::Opus {
                            head: flicks_opus::OpusHead::from_dops(&c.to_bytes()).map_err(Error::decode)?.to_bytes(),
                        },
                        other => M::Other(other.map_or("unknown", C::name).into()),
                    };
                    Ok(Track {
                        id,
                        kind: match t.kind {
                            mp4::TrackKind::Video => TrackKind::Video,
                            mp4::TrackKind::Audio => TrackKind::Audio,
                            mp4::TrackKind::Subtitle => TrackKind::Subtitle,
                            _ => TrackKind::Other,
                        },
                        codec,
                        language: t.language.clone(),
                        name: t.handler_name.clone(),
                        default: t.enabled,
                        enabled: t.timescale != 0,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let cursors = vec![0; tracks.len()];
            Ok((Self::Mp4 { file: Box::new(file), bytes, cursors }, tracks, duration))
        }
    }

    pub fn next(&mut self, selected: &[usize]) -> Result<Option<Packet>> {
        match self {
            Self::Mkv(d) => {
                for _ in 0..1_000_000 {
                    let Some(p) = d.next_packet()? else {
                        if !d.file().warnings.is_empty() {
                            return Err(Error::Media("damaged Matroska stream"));
                        }
                        return Ok(None);
                    };
                    if selected.contains(&p.track) {
                        return Ok(Some(Packet {
                            track: p.track,
                            pts: p.pts_ns,
                            duration: p.duration_ns,
                            discard_padding: p.discard_padding_ns.unwrap_or(0),
                            data: p.data,
                        }));
                    }
                }
                Err(Error::Media("too many unselected packets"))
            }
            Self::Mp4 { file, bytes, cursors } => {
                let next = selected
                    .iter()
                    .filter_map(|&id| {
                        let t = file.tracks.get(id)?;
                        let i = *cursors.get(id)?;
                        let s = t.samples.get(i)?;
                        Some((id, i, ns(s.dts, t.timescale)))
                    })
                    .min_by_key(|(_, _, dts)| *dts);
                let Some((id, i, _)) = next else { return Ok(None) };
                let t = file.tracks.get(id).ok_or(Error::Track)?;
                let s = t.samples.get(i).ok_or(Error::Media("missing sample"))?;
                if s.size > 32 * 1024 * 1024 {
                    return Err(Error::Media("oversized sample"));
                }
                let p = Packet {
                    track: id,
                    pts: ns(s.pts.saturating_add(t.edit_offset), t.timescale),
                    duration: ns(i64::from(s.duration), t.timescale).max(0) as u64,
                    discard_padding: 0,
                    data: file.read_sample(bytes, id, i)?,
                };
                *cursors.get_mut(id).ok_or(Error::Track)? += 1;
                Ok(Some(p))
            }
        }
    }

    pub fn seek(&mut self, video: usize, target: i64) -> Result<()> {
        match self {
            Self::Mkv(d) => {
                d.source().invalidate()?;
                d.seek(video, target)?;
            }
            Self::Mp4 { file, bytes, cursors } => {
                bytes.invalidate()?;
                let v = file.tracks.get(video).ok_or(Error::Track)?;
                let sample = v
                    .sample_at_presentation_time(ticks(target, v.timescale))
                    .ok_or(Error::Media("empty video track"))?;
                let key = v.sync_sample_before(sample);
                let key_ns = ns(v.presentation_pts(key).ok_or(Error::Media("missing keyframe"))?, v.timescale);
                for (id, (t, cursor)) in file.tracks.iter().zip(cursors).enumerate() {
                    *cursor = if id == video {
                        key
                    } else {
                        t.sample_at_presentation_time(ticks(key_ns.saturating_sub(80_000_000), t.timescale))
                            .unwrap_or(0)
                    };
                }
            }
        }
        Ok(())
    }

    /// Fetch the subtitle block indexed at/before the target without decoding video preroll.
    /// The subsequent video seek restores the demuxer's streaming position.
    pub fn subtitle_at(&mut self, track: usize, target: i64) -> Result<Option<Packet>> {
        let Self::Mkv(d) = self else { return Ok(None) };
        if !d.file().cue_keyframes(track).iter().any(|k| k.pts_ns <= target) {
            return Ok(None);
        }
        d.seek(track, target)?;
        for _ in 0..10_000 {
            let Some(p) = d.next_packet()? else { return Ok(None) };
            if p.track == track {
                return Ok(Some(Packet {
                    track,
                    pts: p.pts_ns,
                    duration: p.duration_ns,
                    discard_padding: 0,
                    data: p.data,
                }));
            }
        }
        Err(Error::Media("subtitle seek exceeds packet limit"))
    }
}

fn ns(time: i64, scale: u32) -> i64 {
    (i128::from(time) * 1_000_000_000 / i128::from(scale.max(1))).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}
fn ticks(time: i64, scale: u32) -> i64 {
    (i128::from(time) * i128::from(scale) / 1_000_000_000).clamp(i64::MIN as i128, i64::MAX as i128) as i64
}
