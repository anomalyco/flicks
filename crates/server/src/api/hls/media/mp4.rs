//! Indexed (progressive `moov`) ISO-BMFF episodes for HLS. Direct packets keep
//! the native timescale, edit offset, decode order and sample durations; the
//! caller adds one common shift so every edited DTS is at least zero.
//! Transcode input adds decode context around the same catalog cuts.

use super::{Packet, Track, codec_name, invalid, sps_info};
use flicks_container::isobmff::{self, AvcConfig, CodecConfig, SampleEntry, TrackConfig, TrackKind, cmaf};
use std::{fs::File, io, ops::Range};

/// A two-hour film with several audio tracks has a moov of a few MiB.
const MAX_MOOV_BYTES: u64 = 32 << 20;
/// All tracks together: a day of 48 fps video plus 48 kHz AAC fits.
const MAX_FILE_SAMPLES: usize = 8 << 20;
const MAX_TRACKS: usize = 32;
/// AC-3 frames are at most 3840 bytes, E-AC-3 4096.
const MAX_DOLBY_SAMPLE: u32 = 64 * 1024;
/// Priming context for transcoded audio on either side of the segment.
const AUDIO_PAD_US: i64 = 200_000;

pub(super) struct Source {
    file: File,
    mp4: isobmff::Mp4File,
}

impl Source {
    pub(super) fn open(file: File) -> io::Result<Self> {
        let mp4 = isobmff::open_progressive(&file, MAX_MOOV_BYTES, MAX_FILE_SAMPLES).map_err(invalid)?;
        if mp4.tracks.len() > MAX_TRACKS {
            return Err(invalid("too many tracks"));
        }
        let source = Self { file, mp4 };
        let video = source.video_index()?;
        if !matches!(
            source.mp4.tracks.get(video).and_then(|t| t.codec()),
            Some(CodecConfig::Avc(_) | CodecConfig::Hevc(_))
        ) || source.mp4.tracks[video].display_rotation() != Some(0)
        {
            return Err(invalid("unsupported video"));
        }
        Ok(source)
    }

    /// The catalog indexes the first video track (library::probe::mp4).
    fn video_index(&self) -> io::Result<usize> {
        let i = self.mp4.track_of_kind(TrackKind::Video).ok_or_else(|| invalid("no video"))?;
        if !self.mp4.tracks.get(i).is_some_and(|t| t.enabled) {
            return Err(invalid("catalogued video track disabled"));
        }
        Ok(i)
    }

    /// The catalogued video track, then every supported audio track, enabled
    /// ones first. MP4 alternate audio (other languages) is normally disabled
    /// in `tkhd` but still a rendition. Unsupported or malformed audio is left
    /// out rather than failing the file.
    pub(super) fn tracks(&self) -> io::Result<Vec<Track>> {
        let video = self.video_index()?;
        let mut out = vec![self.track(video)?.ok_or_else(|| invalid("unsupported video"))?];
        let mut audio: Vec<_> =
            self.mp4.tracks.iter().enumerate().filter(|(_, t)| t.kind == TrackKind::Audio).collect();
        audio.sort_by_key(|(_, t)| !t.enabled);
        for (i, _) in audio {
            match self.track(i) {
                Ok(Some(t)) => out.push(t),
                Ok(None) => {}
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    fn track(&self, index: usize) -> io::Result<Option<Track>> {
        let t = self.mp4.tracks.get(index).ok_or_else(|| invalid("no such track"))?;
        let entry = t.entries.first().ok_or_else(|| invalid("no sample description"))?;
        let first = t.samples.first().ok_or_else(|| invalid("empty track"))?;
        if t.timescale == 0 || first.duration == 0 {
            return Err(invalid("track timing"));
        }
        edit_end(self.mp4.timescale, t)?;
        let video = t.kind == TrackKind::Video;
        let entry = match &entry.codec {
            CodecConfig::Avc(_) | CodecConfig::Hevc(_) if video => entry.clone(),
            CodecConfig::Aac(c) if !video => {
                let (object, rate, _) = isobmff::parse_asc(&c.asc).ok_or_else(|| invalid("AAC configuration"))?;
                // Same profile as the Matroska path: LC with 1024-sample frames.
                if object != 2 || rate == 0 || c.asc.get(1).is_none_or(|b| b & 4 != 0) {
                    return Ok(None);
                }
                let channels = entry.audio.as_ref().ok_or_else(|| invalid("missing audio"))?.channels;
                SampleEntry::aac(c.asc.clone(), channels, rate)
            }
            CodecConfig::Ac3 { .. } | CodecConfig::Eac3 { .. } if !video => {
                let enhanced = matches!(entry.codec, CodecConfig::Eac3 { .. });
                if first.size > MAX_DOLBY_SAMPLE {
                    return Err(invalid("Dolby sample too large"));
                }
                let data = self.mp4.read_sample(&self.file, index, 0).map_err(invalid)?;
                // Several syncframes per sample and dependent substreams are refused.
                let Ok((c, rate, _)) = cmaf::dolby_config(&data, enhanced) else {
                    return Ok(None);
                };
                let channels = entry.audio.as_ref().ok_or_else(|| invalid("missing audio"))?.channels;
                let format = if enhanced { *b"ec-3" } else { *b"ac-3" };
                SampleEntry::audio((&format).into(), c, channels, f64::from(rate), 16)
            }
            _ => return Ok(None),
        };
        let mut config = TrackConfig::new(entry, t.timescale);
        config.language = t.language.clone();
        cmaf::validate(&config).map_err(invalid)?;
        let (depth, sps_transfer) = sps_info(&config.entry.codec)?;
        let codec = codec_name(&config.entry.codec)?;
        let track =
            Track { index, config, duration: first.duration, video, depth, sps_transfer, codec, partial_color: None };
        if video {
            // An edit that starts presentation after the first key would hide
            // media the catalog clamps to time zero.
            let key = t.sync_samples().next().ok_or_else(|| invalid("no keyframe"))?;
            if t.presentation_pts(key as usize).is_none_or(|p| p < 0) {
                return Err(invalid("unsupported video edit"));
            }
            if self.decode_delay(&track)? > 16 * u64::from(first.duration) {
                return Err(invalid("decode delay exceeds 16 frames"));
            }
        }
        Ok(Some(track))
    }

    /// Ticks (of `track.config.timescale`) to add so the track's first edited
    /// DTS is not negative. Edited DTS never decreases, so this covers all.
    pub(super) fn decode_delay(&self, track: &Track) -> io::Result<u64> {
        let t = self.mp4.tracks.get(track.index).ok_or_else(|| invalid("no such track"))?;
        let first = t.samples.first().ok_or_else(|| invalid("empty track"))?;
        let dts = i128::from(first.dts) + i128::from(t.edit_offset);
        u64::try_from((-dts).max(0)).map_err(invalid)
    }

    /// Video: catalog key at `start_ms` (floored as library::probe::mp4) up to
    /// the next key at or after `end_ms`, in decode order; transcode adds that
    /// key's leading pictures. Audio: presentation in [start, end), with 200 ms
    /// either side for transcode. Edited native ticks and microseconds.
    pub(super) fn packets(
        &self,
        track: &Track,
        start_ms: u64,
        end_ms: u64,
        transcode: bool,
    ) -> io::Result<Vec<Packet>> {
        let t = self.mp4.tracks.get(track.index).ok_or_else(|| invalid("no such track"))?;
        if t.timescale != track.config.timescale || track.video != (t.kind == TrackKind::Video) || end_ms <= start_ms {
            return Err(invalid("track changed"));
        }
        let range = if track.video {
            video_range(t, start_ms, end_ms, transcode.then_some(track.depth))?
        } else {
            let start = i64::try_from(start_ms.saturating_mul(1000)).map_err(invalid)?;
            let end = i64::try_from(end_ms.saturating_mul(1000)).map_err(invalid)?;
            let (lo, hi) =
                if transcode { (start - AUDIO_PAD_US, end.saturating_add(AUDIO_PAD_US)) } else { (start, end) };
            let hi = match edit_end(self.mp4.timescale, t)? {
                Some(e) => hi.min(us(e, t.timescale)?),
                None => hi,
            };
            let at = |i: usize| us(t.presentation_pts(i).ok_or_else(|| invalid("sample"))?, t.timescale);
            let a = partition(t.samples.len(), |i| Ok(at(i)? < lo))?;
            let b = partition(t.samples.len(), |i| Ok(at(i)? < hi))?;
            a..b.max(a)
        };
        if range.len() > cmaf::MAX_SAMPLES {
            return Err(invalid("segment packet limit"));
        }
        let limit = cmaf::MAX_SEGMENT_BYTES;
        let description = t.entries.first().ok_or_else(|| invalid("no sample description"))?;
        let mut bytes = 0usize;
        let mut packets = Vec::with_capacity(range.len());
        for i in range {
            let s = t.samples.get(i).ok_or_else(|| invalid("sample"))?;
            if s.size > 16 << 20 {
                return Err(invalid("sample too large"));
            }
            bytes = bytes
                .checked_add(s.size as usize)
                .filter(|n| *n <= limit)
                .ok_or_else(|| invalid("segment too large"))?;
            if s.description_index.checked_sub(1).and_then(|d| t.entries.get(d as usize)) != Some(description) {
                return Err(invalid("sample description change"));
            }
            let dts = s.dts.checked_add(t.edit_offset).ok_or_else(|| invalid("timestamp"))?;
            let pts = s.pts.checked_add(t.edit_offset).ok_or_else(|| invalid("timestamp"))?;
            if !track.video && pts != dts {
                return Err(invalid("audio composition offset"));
            }
            let mut data = self.mp4.read_sample(&self.file, track.index, i).map_err(invalid)?;
            match &track.config.entry.codec {
                CodecConfig::Avc(c) => avc_sample(c, &data)?,
                CodecConfig::Hevc(c) => data = cmaf::hvc1_sample(c, &data).map_err(invalid)?,
                c @ CodecConfig::Ac3 { .. } | c @ CodecConfig::Eac3 { .. } => {
                    let enhanced = matches!(c, CodecConfig::Eac3 { .. });
                    if cmaf::dolby_config(&data, enhanced).map_err(invalid)?.0 != *c {
                        return Err(invalid("Dolby configuration change"));
                    }
                }
                _ => {}
            }
            packets.push(Packet {
                data,
                pts_us: us(pts, t.timescale)?,
                keyframe: s.is_sync,
                duration: if s.duration == 0 { track.duration } else { s.duration },
                dts: Some(dts),
                pts: Some(pts),
            });
        }
        Ok(packets)
    }
}

fn video_range(t: &isobmff::Track, start_ms: u64, end_ms: u64, depth: Option<u32>) -> io::Result<Range<usize>> {
    let len = t.samples.len();
    let sync: Vec<u32> = t.sync_samples().collect();
    let pts = |i: usize| t.presentation_pts(i).ok_or_else(|| invalid("sample"));
    let ms = |k: usize| -> io::Result<u64> {
        let i = *sync.get(k).ok_or_else(|| invalid("keyframe"))?;
        Ok(u64::try_from(pts(i as usize)?).unwrap_or(0).saturating_mul(1000) / u64::from(t.timescale))
    };
    let a = partition(sync.len(), |k| Ok(ms(k)? < start_ms))?;
    let first = *sync.get(a).ok_or(io::ErrorKind::NotFound)? as usize;
    if start_ms > 0 && ms(a)? != start_ms {
        return Err(invalid("keyframe index changed since scan"));
    }
    let b = partition(sync.len(), |k| Ok(ms(k)? < end_ms))?;
    let stop = sync.get(b).map_or(len, |i| *i as usize);
    let mut end = stop.max(first);
    if let Some(depth) = depth
        && stop < len
    {
        // Open-GOP leading pictures follow the next key in decode order but
        // present before it, inside this segment.
        let key = pts(stop)?;
        let mut trailing = 0;
        for i in stop..len {
            if i - stop >= cmaf::MAX_SAMPLES {
                return Err(invalid("open GOP too large"));
            }
            if pts(i)? < key {
                end = i + 1;
            } else {
                trailing += 1;
                if trailing > depth {
                    break;
                }
            }
        }
    }
    Ok(first..end)
}

/// Leading empty edits then at most one normal-rate edit: a constant offset
/// (the demuxer's `edit_offset`). The edited end in track ticks, if bounded.
fn edit_end(movie_timescale: u32, t: &isobmff::Track) -> io::Result<Option<i64>> {
    let mut empty = 0u128;
    let mut end = None;
    let mut media = false;
    for e in &t.edits {
        if media {
            return Err(invalid("unsupported edit list"));
        }
        if e.media_time == -1 {
            empty += u128::from(e.segment_duration);
            continue;
        }
        if e.media_rate != 0x1_0000 || e.media_time < 0 {
            return Err(invalid("unsupported edit"));
        }
        media = true;
        if e.segment_duration > 0 {
            end = Some(empty + u128::from(e.segment_duration));
        }
    }
    if t.edits.is_empty() {
        return Ok(None);
    }
    if !media || movie_timescale == 0 {
        return Err(invalid("unsupported edit list"));
    }
    end.map(|e| i64::try_from(e * u128::from(t.timescale) / u128::from(movie_timescale)).map_err(invalid)).transpose()
}

fn us(ticks: i64, timescale: u32) -> io::Result<i64> {
    if timescale == 0 {
        return Err(invalid("timescale"));
    }
    i64::try_from((i128::from(ticks) * 1_000_000).div_euclid(i128::from(timescale))).map_err(invalid)
}

/// First index in `0..n` for which `below` is false; `below` must be monotonic.
fn partition(n: usize, mut below: impl FnMut(usize) -> io::Result<bool>) -> io::Result<usize> {
    let (mut lo, mut hi) = (0, n);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if below(mid)? {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// avc1 output carries the avcC parameter sets; in-band copies must match them.
fn avc_sample(c: &AvcConfig, data: &[u8]) -> io::Result<()> {
    let size = usize::from(c.length_size);
    if !matches!(size, 1 | 2 | 4) {
        return Err(invalid("AVC NAL length size"));
    }
    let mut pos = 0usize;
    for _ in 0..4096 {
        if pos == data.len() {
            return Ok(());
        }
        let start = pos.checked_add(size).ok_or_else(|| invalid("AVC NAL"))?;
        let prefix = data.get(pos..start).ok_or_else(|| invalid("AVC NAL length"))?;
        let len = prefix.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
        let end = start.checked_add(len).ok_or_else(|| invalid("AVC NAL"))?;
        let nal = data.get(start..end).ok_or_else(|| invalid("AVC NAL"))?;
        let kind = nal.first().ok_or_else(|| invalid("empty AVC NAL"))? & 0x1f;
        if (kind == 7 && !c.sps.iter().any(|s| s == nal)) || (kind == 8 && !c.pps.iter().any(|p| p == nal)) {
            return Err(invalid("in-band AVC configuration change"));
        }
        pos = end;
    }
    Err(invalid("AVC NAL count"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ffmpeg(ffmpeg: &Path, args: &[&str], out: &Path) {
        let output =
            std::process::Command::new(ffmpeg).args(["-v", "error", "-y"]).args(args).arg(out).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    /// Two-second GOPs with B-frames, AAC (priming edit) and AC-3.
    fn episode(ffmpeg_path: &Path, dir: &Path, encoder: &str, extra: &[&str]) -> std::path::PathBuf {
        let path = dir.join(format!("{encoder}.mp4"));
        let mut args = vec![
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x90:rate=24:duration=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000:duration=10",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "1:a",
            "-c:v",
            encoder,
            "-g",
            "48",
            "-bf",
            "2",
            "-pix_fmt",
            "yuv420p",
            "-c:a:0",
            "aac",
            "-c:a:1",
            "ac3",
        ];
        args.extend_from_slice(extra);
        ffmpeg(ffmpeg_path, &args, &path);
        path
    }

    /// Catalog keys exactly as library::probe::mp4 records them.
    fn cuts(source: &Source) -> Vec<u64> {
        let t = &source.mp4.tracks[source.video_index().unwrap()];
        let mut keys: Vec<flicks_library::Keyframe> = Vec::new();
        for i in t.sync_samples() {
            let ms =
                u64::try_from(t.presentation_pts(i as usize).unwrap()).unwrap_or(0) * 1000 / u64::from(t.timescale);
            if keys.last().is_none_or(|k| ms >= u64::from(k.time_ms) + 1000) {
                keys.push(flicks_library::Keyframe { time_ms: ms as u32, offset: 0 });
            }
        }
        let duration = source.mp4.duration * 1000 / u64::from(source.mp4.timescale);
        super::super::super::boundaries(&keys, duration).unwrap()
    }

    #[test]
    fn direct_segments_partition_samples_and_transcode_adds_context() {
        let (ffmpeg_path, _) = flicks_testkit::require_oracles!();
        let dir = tempfile::tempdir().unwrap();
        for (encoder, extra) in [("libx264", &[][..]), ("libx265", &["-x265-params", "log-level=error:open-gop=1"][..])]
        {
            let path = episode(&ffmpeg_path, dir.path(), encoder, extra);
            let source = Source::open(File::open(&path).unwrap()).unwrap();
            let tracks = source.tracks().unwrap();
            assert!(tracks[0].video);
            assert_eq!(tracks.len(), 3, "{encoder}: video, AAC, AC-3");
            assert!(matches!(tracks[2].config.entry.codec, CodecConfig::Ac3 { .. }));
            let cuts = cuts(&source);
            assert!(cuts.len() >= 3, "{cuts:?}");
            let video = &tracks[0];
            // B-frames make the first edited DTS negative; the caller's
            // reorder shift has to cover it.
            let delay = source.decode_delay(video).unwrap();
            assert!(delay > 0 && delay <= u64::from(video.depth) * u64::from(video.duration), "{encoder}: {delay}");
            let mut lookahead = 0;
            for track in &tracks {
                let t = &source.mp4.tracks[track.index];
                let mut next = None;
                let mut audio_end = None;
                for w in cuts.windows(2) {
                    let direct = source.packets(track, w[0], w[1], false).unwrap();
                    let transcode = source.packets(track, w[0], w[1], true).unwrap();
                    assert!(!direct.is_empty());
                    if track.video {
                        // Contiguous decode order from key to key, durations kept.
                        let first = direct[0].dts.unwrap();
                        assert!(direct[0].keyframe && direct[0].pts_us / 1000 == w[0] as i64 || w[0] == 0);
                        assert_eq!(next.unwrap_or(first), first);
                        next = Some(direct.iter().fold(first, |d, p| {
                            assert_eq!(p.dts.unwrap(), d);
                            d + i64::from(p.duration)
                        }));
                        // Every picture presented in [start, end) is decodable input.
                        let wanted = (0..t.samples.len())
                            .filter(|&i| {
                                let us = us(t.presentation_pts(i).unwrap(), t.timescale).unwrap();
                                us >= w[0] as i64 * 1000 && us < w[1] as i64 * 1000
                            })
                            .count();
                        let have = transcode
                            .iter()
                            .filter(|p| p.pts_us >= w[0] as i64 * 1000 && p.pts_us < w[1] as i64 * 1000)
                            .count();
                        assert_eq!(have, wanted, "{encoder} {w:?}");
                        assert!(transcode.len() >= direct.len());
                        lookahead += transcode.len() - direct.len();
                        assert!(transcode.iter().zip(&direct).all(|(a, b)| a.dts == b.dts && a.data == b.data));
                    } else {
                        assert!(direct.iter().all(|p| p.pts_us >= w[0] as i64 * 1000 && p.pts_us < w[1] as i64 * 1000));
                        if let Some(end) = audio_end {
                            assert_eq!(direct[0].pts.unwrap(), end, "audio gap or overlap at {}", w[0]);
                        }
                        let last = direct.last().unwrap();
                        audio_end = Some(last.pts.unwrap() + i64::from(last.duration));
                        if w[0] > 0 {
                            assert!(transcode[0].pts_us <= w[0] as i64 * 1000 - 150_000);
                        }
                        assert!(transcode.len() > direct.len());
                    }
                }
                if track.video {
                    // x265's open GOPs put leading pictures after each CRA.
                    assert_eq!(lookahead > 0, encoder == "libx265", "{encoder}: {lookahead}");
                    assert_eq!(
                        next,
                        Some(
                            t.samples.iter().map(|s| i64::from(s.duration)).sum::<i64>()
                                + t.samples.first().unwrap().dts
                                + t.edit_offset
                        )
                    );
                }
            }
        }
    }

    #[test]
    fn zero_and_one_tick_mp4_tails_keep_the_last_picture() {
        use isobmff::{Brand, Mp4Writer, WriteSample, WriterOptions};
        let (oracle, _) = flicks_testkit::require_oracles!();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.mp4");
        ffmpeg(
            &oracle,
            &["-f", "lavfi", "-i", "testsrc2=size=160x96:rate=25", "-frames:v", "3", "-c:v", "libx264", "-bf", "0"],
            &path,
        );
        let file = File::open(path).unwrap();
        let mp4 = isobmff::open(&file).unwrap();
        // Two pictures share the final PTS. On the output decode clock VT separates them by 1 µs.
        for tail in [0, 1] {
            let mut writer = Mp4Writer::new(tempfile::tempfile().unwrap(), WriterOptions::new(Brand::Mp4)).unwrap();
            let t = writer.add_track(TrackConfig::new(mp4.tracks[0].entries[0].clone(), 1_000_000)).unwrap();
            for (i, duration) in [40_000, 40_000, tail].into_iter().enumerate() {
                let data = mp4.read_sample(&file, 0, i).unwrap();
                writer
                    .write_sample(
                        t,
                        WriteSample {
                            data: &data,
                            duration,
                            composition_offset: if i == 2 { -40_000 } else { 0 },
                            is_sync: i == 0,
                        },
                    )
                    .unwrap();
            }
            let source = Source::open(writer.finish().unwrap()).unwrap();
            let track = &source.tracks().unwrap()[0];
            let packets = source.packets(track, 0, 100, false).unwrap();
            assert_eq!(packets.len(), 3);
            assert_eq!(packets[2].pts_us, packets[1].pts_us);
            assert_eq!(packets[2].duration, if tail == 0 { track.duration } else { tail });
            super::super::remux(track, &packets, 1, 0).unwrap();
            #[cfg(target_os = "macos")]
            {
                use super::super::{Rendition, Tier, video};
                let tier = Tier { name: Rendition::Video(480), width: 128, height: 72, bitrate: 1_200_000 };
                let (mut init, segment) = video::segment(
                    track,
                    tier,
                    &packets,
                    1,
                    0..100,
                    0,
                    std::time::Instant::now() + super::super::DEADLINE,
                )
                .unwrap();
                init.extend(segment);
                let output = isobmff::open(&init).unwrap();
                let last = output.tracks[0].samples.get(2).unwrap();
                assert_eq!((last.dts, last.pts), (40_001, 40_000));
                assert_eq!(last.duration, if tail == 0 { 39_999 } else { 1 });
            }
        }
    }

    #[test]
    fn fragments_and_rate_edits_are_refused() {
        let (ffmpeg_path, _) = flicks_testkit::require_oracles!();
        let dir = tempfile::tempdir().unwrap();
        let frag = dir.path().join("frag.mp4");
        ffmpeg(
            &ffmpeg_path,
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x90:rate=24:duration=2",
                "-c:v",
                "libx264",
                "-movflags",
                "frag_keyframe+empty_moov",
            ],
            &frag,
        );
        assert!(Source::open(File::open(&frag).unwrap()).is_err());
        let path = episode(&ffmpeg_path, dir.path(), "libx264", &[]);
        let mut bytes = std::fs::read(&path).unwrap();
        let elst = bytes.windows(4).position(|w| w == b"elst").unwrap();
        // version 0: version/flags, count, then duration, media_time, rate.
        let rate = elst + 4 + 4 + 4 + 8;
        bytes[rate..rate + 4].copy_from_slice(&0x2_0000i32.to_be_bytes());
        let edited = dir.path().join("rate.mp4");
        std::fs::write(&edited, &bytes).unwrap();
        let source = Source::open(File::open(&edited).unwrap()).unwrap();
        assert!(source.tracks().is_err());
    }

    #[test]
    fn truncated_and_flipped_files_never_panic() {
        let (ffmpeg_path, _) = flicks_testkit::require_oracles!();
        let dir = tempfile::tempdir().unwrap();
        let path = episode(&ffmpeg_path, dir.path(), "libx264", &["-movflags", "+faststart"]);
        let original = std::fs::read(&path).unwrap();
        let moov = original.windows(4).position(|w| w == b"moov").unwrap();
        let mut cases = Vec::new();
        for n in (0..original.len()).step_by(original.len() / 64) {
            cases.push(original[..n].to_vec());
        }
        let mut seed = 0x2545_f491_u64;
        for _ in 0..256 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let mut b = original.clone();
            // Mostly the header region, where table and size fields live.
            let at = moov.saturating_sub(4) + (seed as usize >> 8) % 4096;
            if let Some(byte) = b.get_mut(at) {
                *byte ^= 1 << (seed % 8);
            }
            cases.push(b);
        }
        let case = dir.path().join("case.mp4");
        let mut opened = 0;
        for bytes in cases {
            std::fs::write(&case, &bytes).unwrap();
            let Ok(source) = Source::open(File::open(&case).unwrap()) else {
                continue;
            };
            opened += 1;
            let Ok(tracks) = source.tracks() else {
                continue;
            };
            for track in &tracks {
                for (start, end) in [(0, 4000), (4000, 8000), (8000, 10_000), (0, u64::MAX / 2)] {
                    let _ = source.packets(track, start, end, false);
                    let _ = source.packets(track, start, end, true);
                }
            }
        }
        assert!(opened > 0);
    }
}
