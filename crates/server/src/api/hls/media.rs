use super::{boundaries, cmaf, invalid, playlist};
use flicks_container::{
    isobmff::{self, CodecConfig, SampleEntry, TrackConfig, WriteSample},
    matroska::{self, Codec, Demuxer, Packet},
};
use std::{fs::File, io};

const TS: u32 = 1_000_000;
const PLAYLIST: &str = "application/vnd.apple.mpegurl";

struct Track {
    index: usize,
    config: TrackConfig,
    duration: u32,
    video: bool,
    depth: u32,
    codec: String,
}

fn config(t: &matroska::Track, index: usize, packet: Option<&Packet>) -> io::Result<Option<Track>> {
    if !t.enabled || !t.frames_readable() {
        return Ok(None);
    }
    let video = t.video.is_some();
    let (entry, duration, codec) = match &t.codec {
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
            let (entry, codec) = if matches!(t.codec, Codec::Avc { .. }) {
                let c = isobmff::AvcConfig::parse(avcc).map_err(invalid)?;
                let codec = format!("avc1.{:02x}{:02x}{:02x}", c.profile, c.compatibility, c.level);
                (SampleEntry::avc(c, w, h), codec)
            } else {
                let c = isobmff::HevcConfig::parse(avcc).map_err(invalid)?;
                let space = match c.general_profile_space {
                    1 => "A",
                    2 => "B",
                    3 => "C",
                    _ => "",
                };
                let mut codec = format!(
                    "hvc1.{space}{}.{:X}.{}{}",
                    c.general_profile_idc,
                    c.general_profile_compatibility_flags.reverse_bits(),
                    if c.general_tier_flag { "H" } else { "L" },
                    c.general_level_idc
                );
                let bytes = c.general_constraint_indicator_flags.to_be_bytes();
                let constraints = bytes.get(2..).ok_or_else(|| invalid("HEVC constraints"))?;
                let last = constraints.iter().rposition(|b| *b != 0);
                if let Some(last) = last {
                    for b in constraints.iter().take(last + 1) {
                        codec.push_str(&format!(".{b:02X}"));
                    }
                }
                (SampleEntry::hevc(c, w, h), codec)
            };
            (entry, (duration / 1000) as u32, codec)
        }
        Codec::Aac { asc } => {
            let (object, rate, _) = isobmff::parse_asc(asc).ok_or_else(|| invalid("AAC configuration"))?;
            // HE-AAC/960-sample frames require different packet durations.
            if object != 2 || rate == 0 || asc.get(1).is_none_or(|b| b & 4 != 0) {
                return Ok(None);
            }
            let a = t.audio.as_ref().ok_or_else(|| invalid("missing audio"))?;
            (
                SampleEntry::aac(asc.clone(), u32::try_from(a.channels).map_err(invalid)?, rate),
                (1024u64 * u64::from(TS) / u64::from(rate)) as u32,
                "mp4a.40.2".into(),
            )
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
                (u64::from(samples) * u64::from(TS) / u64::from(rate)) as u32,
                if enhanced { "ec-3" } else { "ac-3" }.into(),
            )
        }
        _ => return Ok(None),
    };
    let depth = cmaf::timing::reorder_depth(&entry.codec).map_err(invalid)?;
    let mut config = TrackConfig::new(entry, TS);
    config.language = t.language.clone();
    cmaf::write_init_segment(std::slice::from_ref(&config)).map_err(invalid)?;
    Ok(Some(Track { index, config, duration, video, depth, codec }))
}

pub(super) fn render(
    item: flicks_library::Item,
    keys: Vec<flicks_library::Keyframe>,
    resource: &str,
    suffix: &str,
) -> io::Result<(&'static str, Vec<u8>)> {
    let file = super::super::files::open(&item.root, &item.path)?;
    if file.metadata()?.len() != item.size {
        return Err(invalid("file changed since scan"));
    }
    let cuts = boundaries(&keys, item.duration_ms.ok_or_else(|| invalid("missing duration"))?)?;
    let mut demux = Demuxer::new(file).map_err(invalid)?;
    if demux.file().cues.is_empty() {
        return Err(invalid("Cues required"));
    }
    if demux.tracks().len() > 32 {
        return Err(invalid("too many tracks"));
    }
    let mut first_dolby = std::collections::BTreeMap::new();
    let dolby = demux.tracks().iter().filter(|t| matches!(t.codec, Codec::Ac3 | Codec::Eac3)).count();
    if dolby > 0 {
        for _ in 0..4096 {
            let Some(p) = demux.next_packet().map_err(invalid)? else {
                break;
            };
            if demux.tracks().get(p.track).is_some_and(|t| matches!(t.codec, Codec::Ac3 | Codec::Eac3)) {
                first_dolby.entry(p.track).or_insert(p);
            }
            if first_dolby.len() == dolby {
                break;
            }
        }
        demux.rewind();
    }
    let mut tracks = Vec::new();
    for (i, t) in demux.tracks().iter().enumerate() {
        if let Some(t) = config(t, i, first_dolby.get(&i))? {
            tracks.push(t);
        }
    }
    let video = tracks.iter().find(|t| t.video).ok_or_else(|| invalid("no supported video"))?;
    let delay = u64::from(video.duration) * u64::from(video.depth);
    if resource == "master.m3u8" {
        let audio: Vec<_> = tracks.iter().filter(|t| !t.video).collect();
        let mut out = "#EXTM3U\n#EXT-X-VERSION:7\n".to_string();
        for (i, t) in audio.iter().enumerate() {
            out.push_str(&format!("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"Audio {}\",DEFAULT={},AUTOSELECT=YES,URI=\"{}.m3u8{suffix}\"\n", i + 1, if i == 0 { "YES" } else { "NO" }, t.index));
        }
        let codecs = std::iter::once(video.codec.as_str())
            .chain(audio.iter().map(|t| t.codec.as_str()))
            .collect::<Vec<_>>()
            .join(",");
        let bandwidth =
            item.size.saturating_mul(8000).checked_div(*cuts.last().unwrap_or(&1)).unwrap_or(1).saturating_mul(2);
        let audio = if audio.is_empty() { "" } else { ",AUDIO=\"audio\"" };
        out.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},CODECS=\"{codecs}\"{audio}\n{}.m3u8{suffix}\n",
            video.index
        ));
        return Ok((PLAYLIST, out.into_bytes()));
    }
    if let Some(track) = resource.strip_suffix(".m3u8").and_then(|s| s.parse::<usize>().ok()) {
        if !tracks.iter().any(|t| t.index == track) {
            return Err(io::ErrorKind::NotFound.into());
        }
        return Ok((PLAYLIST, playlist(&cuts, track, suffix).into_bytes()));
    }
    let (track, name) = resource.split_once('/').ok_or(io::ErrorKind::NotFound)?;
    let track = track.parse::<usize>().map_err(|_| io::ErrorKind::NotFound)?;
    let track = tracks.iter().find(|t| t.index == track).ok_or(io::ErrorKind::NotFound)?;
    if name == "init.mp4" {
        return Ok(("video/mp4", cmaf::write_init_segment(std::slice::from_ref(&track.config)).map_err(invalid)?));
    }
    let seq = name.strip_suffix(".m4s").and_then(|s| s.parse::<usize>().ok()).ok_or(io::ErrorKind::NotFound)?;
    let start = *cuts.get(seq).ok_or(io::ErrorKind::NotFound)?;
    let end = *cuts.get(seq.checked_add(1).ok_or(io::ErrorKind::NotFound)?).ok_or(io::ErrorKind::NotFound)?;
    let data = segment(&mut demux, track, video.index, seq, start, end, delay)?;
    Ok(("video/mp4", data))
}

fn segment(
    demux: &mut Demuxer<File>,
    track: &Track,
    video: usize,
    seq: usize,
    start: u64,
    end: u64,
    delay: u64,
) -> io::Result<Vec<u8>> {
    // Audio may precede the indexed video block in a Cluster. Seek to the prior
    // cue and filter by PTS, rather than silently dropping those audio frames.
    if start > 0 {
        demux.seek(video, (start.saturating_sub(1) * 1_000_000) as i64).map_err(invalid)?;
    }
    let mut packets = Vec::new();
    let mut size = 0;
    let mut begun = !track.video;
    let mut ended = false;
    for _ in 0..cmaf::MAX_SAMPLES {
        let Some(mut p) = demux.next_packet().map_err(invalid)? else {
            ended = true;
            break;
        };
        if p.track != track.index {
            continue;
        }
        let time = p.pts_ns / 1_000_000;
        if track.video {
            if p.keyframe && time >= end as i64 {
                ended = true;
                break;
            }
            if !begun && p.keyframe && time >= start as i64 {
                begun = true;
            }
            if !begun {
                continue;
            }
        } else {
            if time >= end as i64 {
                ended = true;
                break;
            }
            // CodecDelay is already subtracted by the demuxer. Drop complete
            // priming frames before presentation zero; no edit-list machinery.
            if p.pts_ns < (start * 1_000_000) as i64 {
                continue;
            }
        }
        size += p.data.len();
        if size > cmaf::MAX_SEGMENT_BYTES {
            return Err(invalid("segment too large"));
        }
        if let CodecConfig::Hevc(c) = &track.config.entry.codec {
            p.data = cmaf::hvc1_sample(c, &p.data).map_err(invalid)?;
        }
        packets.push(p);
    }
    if !ended {
        return Err(invalid("segment packet limit"));
    }
    let first = packets.first().ok_or_else(|| invalid("empty segment"))?;
    let start_pts = first.pts_ns / 1000;
    let mut samples = Vec::with_capacity(packets.len());
    for (i, p) in packets.iter().enumerate() {
        let offset = if track.video {
            cmaf::timing::timestamp(start_pts, p.pts_ns / 1000, i, track.duration, track.depth).map_err(invalid)?.1
        } else {
            0
        };
        samples.push(WriteSample {
            data: &p.data,
            duration: track.duration,
            composition_offset: offset,
            is_sync: p.keyframe,
        });
    }
    // Retained audio starts at or after zero and shares video's reorder delay.
    let base =
        u64::try_from(i128::from(start_pts) + if track.video { 0 } else { i128::from(delay) }).map_err(invalid)?;
    cmaf::write_media_segment(u32::try_from(seq + 1).map_err(invalid)?, base, &samples).map_err(invalid)
}
