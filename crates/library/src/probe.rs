//! Reads one media file's headers with `container`, and its OSHash. The file is opened by the
//! caller, once.

use std::fs::File;
use std::io;

use flicks_container::isobmff::{self, CodecConfig, ColorInfo};
use flicks_container::matroska::{self, ByteSource, TrackKind};

use crate::catalog::{Keyframe, Stream, StreamKind};

pub(crate) struct Video {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub hdr: Option<&'static str>,
}

pub(crate) struct Probed {
    pub size: u64,
    pub oshash: u64,
    pub container: &'static str,
    pub duration_ms: Option<u64>,
    pub video: Option<Video>,
    pub streams: Vec<Stream>,
    pub keyframes: Vec<Keyframe>,
}

/// Seek granularity is a second; an all-intra file would otherwise index every frame.
const MIN_KEYFRAME_GAP_MS: u64 = 1000;

pub(crate) fn probe(file: &File) -> io::Result<Probed> {
    let size = ByteSource::len(file);
    let oshash = oshash(file, size)?;
    let mut magic = [0u8; 4];
    file.read_at(0, &mut magic)?;
    let parts = if magic == [0x1A, 0x45, 0xDF, 0xA3] { mkv(file)? } else { mp4(file)? };
    Ok(Probed {
        size,
        oshash,
        container: parts.container,
        duration_ms: parts.duration_ms,
        video: parts.video,
        streams: parts.streams,
        keyframes: parts.keyframes,
    })
}

/// The OpenSubtitles hash: the file size plus every little-endian u64 of the first and the last
/// 64 KiB, wrapping. A file shorter than 64 KiB hashes what it has.
pub(crate) fn oshash(file: &File, size: u64) -> io::Result<u64> {
    const CHUNK: u64 = 64 * 1024;
    let n = size.min(CHUNK);
    let mut hash = size;
    let mut buf = vec![0u8; CHUNK as usize];
    for start in [0, size - n] {
        let used = &mut buf[..n as usize];
        file.read_at(start, used)?;
        for word in used.as_chunks::<8>().0 {
            hash = hash.wrapping_add(u64::from_le_bytes(*word));
        }
    }
    Ok(hash)
}

struct Parts {
    container: &'static str,
    duration_ms: Option<u64>,
    video: Option<Video>,
    streams: Vec<Stream>,
    keyframes: Vec<Keyframe>,
}

/// Collects keyframes in time order, at most one per [`MIN_KEYFRAME_GAP_MS`].
#[derive(Default)]
struct Keyframes(Vec<Keyframe>);

impl Keyframes {
    fn push(&mut self, time_ms: u64, offset: u64) {
        if self.0.last().is_none_or(|k| time_ms >= u64::from(k.time_ms) + MIN_KEYFRAME_GAP_MS) {
            self.0.push(Keyframe { time_ms: u32::try_from(time_ms).unwrap_or(u32::MAX), offset });
        }
    }
}

fn hdr(transfer: Option<u64>) -> Option<&'static str> {
    match transfer? {
        16 => Some("hdr10"),
        18 => Some("hlg"),
        _ => None,
    }
}

fn mkv(file: &File) -> io::Result<Parts> {
    let mut demux = matroska::Demuxer::new(file).map_err(io::Error::other)?;
    let doc = demux.file();
    let duration_ms = doc.duration_ns().map(|ns| ns / 1_000_000);
    let video_index = demux.tracks().iter().position(|t| t.kind == TrackKind::Video);
    let video = video_index.and_then(|i| demux.tracks().get(i)).and_then(|t| {
        let v = t.video.as_ref()?;
        Some(Video {
            codec: t.codec.name(),
            width: v.pixel_width,
            height: v.pixel_height,
            hdr: hdr(v.color.as_ref().and_then(|c| c.transfer_characteristics)),
        })
    });
    let streams = demux
        .tracks()
        .iter()
        .filter_map(|t| {
            let kind = match t.kind {
                TrackKind::Audio => StreamKind::Audio,
                TrackKind::Subtitle => StreamKind::Subtitle,
                _ => return None,
            };
            Some(Stream {
                kind,
                codec: t.codec.name(),
                language: t.language.clone(),
                title: t.name.clone(),
                channels: t.audio.as_ref().map_or(0, |a| u32::try_from(a.channels).unwrap_or(0)),
                default: t.default,
                forced: t.forced,
            })
        })
        .collect();
    // Only from Cues: without them the demuxer would scan every Cluster, and a library scan
    // never does that. Such a file has no index and a player builds one on open.
    let mut keyframes = Keyframes::default();
    if let Some(i) = video_index
        && let Some(number) = demux.tracks().get(i).map(|t| t.number)
        && demux.file().cues.iter().any(|c| c.positions.iter().any(|p| p.track == number))
    {
        for k in demux.keyframe_index(i).map_err(io::Error::other)? {
            keyframes.push(u64::try_from(k.pts_ns).unwrap_or(0) / 1_000_000, k.cluster_offset);
        }
    }
    Ok(Parts { container: "mkv", duration_ms, video, streams, keyframes: keyframes.0 })
}

fn mp4(file: &File) -> io::Result<Parts> {
    let mp4 = isobmff::open(file).map_err(io::Error::other)?;
    let ms = |ticks: u64, timescale: u32| (timescale != 0).then(|| ticks.saturating_mul(1000) / u64::from(timescale));
    let duration_ms = ms(mp4.duration, mp4.timescale);
    let video_track = mp4.tracks.iter().find(|t| t.kind == isobmff::TrackKind::Video);
    let video = video_track.and_then(|t| {
        let entry = t.entries.first()?;
        let params = entry.video.as_ref();
        let transfer = match params.and_then(|p| p.color.as_ref()) {
            Some(ColorInfo::Nclx { transfer, .. } | ColorInfo::Nclc { transfer, .. }) => Some(u64::from(*transfer)),
            _ => None,
        };
        let hdr = hdr(transfer).or_else(|| params.and_then(|p| p.mastering_display).map(|_| "hdr10"));
        Some(Video {
            codec: codec_name(&entry.codec, &entry.format.to_string_lossy()),
            width: params.map_or(t.width, |p| u32::from(p.width)),
            height: params.map_or(t.height, |p| u32::from(p.height)),
            hdr,
        })
    });
    let streams = mp4
        .tracks
        .iter()
        .filter_map(|t| {
            let kind = match t.kind {
                isobmff::TrackKind::Audio => StreamKind::Audio,
                isobmff::TrackKind::Subtitle => StreamKind::Subtitle,
                _ => return None,
            };
            let entry = t.entries.first()?;
            Some(Stream {
                kind,
                codec: codec_name(&entry.codec, &entry.format.to_string_lossy()),
                language: t.language.clone(),
                title: t.handler_name.clone(),
                channels: t.audio().map_or(0, |a| a.channels),
                default: false,
                forced: false,
            })
        })
        .collect();
    let mut keyframes = Keyframes::default();
    if let Some(t) = video_track {
        for index in t.sync_samples() {
            let (Some(pts), Some(sample)) = (t.presentation_pts(index as usize), t.samples.get(index as usize)) else {
                continue;
            };
            keyframes.push(ms(u64::try_from(pts).unwrap_or(0), t.timescale).unwrap_or(0), sample.offset);
        }
    }
    Ok(Parts { container: "mp4", duration_ms, video, streams, keyframes: keyframes.0 })
}

fn codec_name(codec: &CodecConfig, format: &str) -> String {
    match codec {
        CodecConfig::Unknown { .. } => format.trim().to_ascii_lowercase(),
        c => c.name().to_string(),
    }
}
