//! HLS media is still cut once from the catalog index. Each selected rendition
//! reads just that cut in decode order; the output timestamps share one delay.
mod mkv;
mod mp4;
mod rendition;
#[cfg(target_os = "macos")]
mod stereo;
#[cfg(target_os = "macos")]
mod video;

use super::{CancelOnDrop, Work, boundaries, cmaf, invalid, permits, playlist};
use crate::server::State;
use flicks_container::{
    bitstream::unescape_rbsp,
    isobmff::{CodecConfig, TrackConfig, WriteSample},
};
use hyper::body::Bytes;
pub(crate) use rendition::Cache;
use rendition::Rendition;
pub(super) use rendition::Resource;
#[cfg(target_os = "macos")]
use rendition::Tier;
use std::{
    fs::File,
    io,
    time::{Duration, Instant},
};

const TS: u32 = 1_000_000;
const PLAYLIST: &str = "application/vnd.apple.mpegurl";
const MAX_SCAN_BYTES: usize = 256 << 20;
pub(super) const DEADLINE: Duration = Duration::from_secs(12);

struct Track {
    index: usize,
    config: TrackConfig,
    duration: u32,
    video: bool,
    depth: u32,
    sps_transfer: u8,
    codec: String,
    // A partial Matroska Colour is useful for pixel transfer, but must not
    // write an MP4 colr that overrides known SPS values with "unspecified".
    partial_color: Option<flicks_container::isobmff::ColorInfo>,
}
impl Track {
    fn source_color(&self) -> Option<&flicks_container::isobmff::ColorInfo> {
        self.partial_color.as_ref().or_else(|| self.config.entry.video.as_ref()?.color.as_ref())
    }
}

fn sps_info(codec: &CodecConfig) -> io::Result<(u32, u8)> {
    let mut depth = 0;
    let mut trc = None;
    let mut record = |n: Option<u8>| {
        let n = n.unwrap_or(2);
        trc = Some(if trc.is_none_or(|p| p == n) { n } else { 2 });
    };
    match codec {
        CodecConfig::Avc(c) => {
            for nal in &c.sps {
                let s =
                    flicks_h264::params::Sps::parse(&unescape_rbsp(nal.get(1..).ok_or_else(|| invalid("AVC SPS"))?))
                        .map_err(invalid)?;
                if !s.frame_mbs_only {
                    return Err(invalid("interlaced video"));
                }
                record(s.vui.as_ref().map(|v| v.transfer_characteristics));
                depth = depth.max(s.max_num_reorder_frames().min(s.max_dpb_frames()) as u32);
            }
        }
        CodecConfig::Hevc(c) => {
            for nal in c.arrays.iter().filter(|a| a.nal_type == 33).flat_map(|a| &a.nalus) {
                let s =
                    flicks_hevc::params::Sps::parse(&unescape_rbsp(nal.get(2..).ok_or_else(|| invalid("HEVC SPS"))?))
                        .map_err(invalid)?;
                if s.vui.as_ref().is_some_and(|v| v.field_seq) {
                    return Err(invalid("interlaced video"));
                }
                record(s.vui.as_ref().map(|v| v.transfer_characteristics));
                depth = depth.max(s.max_num_reorder);
            }
        }
        _ => {}
    }
    if depth > 16 {
        return Err(invalid("reorder depth exceeds 16"));
    }
    Ok((depth, trc.unwrap_or(2)))
}

/// For Matroska, only PTS is stored; for MP4 we also have exact native clocks.
struct Packet {
    data: Vec<u8>,
    pts_us: i64,
    keyframe: bool,
    duration: u32,
    dts: Option<i64>,
    pts: Option<i64>,
}

fn codec_name(c: &CodecConfig) -> io::Result<String> {
    Ok(match c {
        CodecConfig::Avc(c) => format!("avc1.{:02x}{:02x}{:02x}", c.profile, c.compatibility, c.level),
        CodecConfig::Hevc(c) => {
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
            let constraints = &bytes[2..];
            if let Some(last) = constraints.iter().rposition(|n| *n != 0) {
                for b in &constraints[..=last] {
                    codec.push_str(&format!(".{b:02X}"));
                }
            }
            codec
        }
        CodecConfig::Aac(_) => "mp4a.40.2".into(),
        CodecConfig::Ac3 { .. } => "ac-3".into(),
        CodecConfig::Eac3 { .. } => "ec-3".into(),
        _ => return Err(invalid("unsupported HLS codec")),
    })
}

enum Source {
    Mkv(Box<mkv::Source>),
    Mp4(mp4::Source),
}

struct Opened {
    file: File,
    item: flicks_library::Item,
    key: Option<rendition::FileKey>,
}

pub(super) async fn render(
    state: &State,
    item: flicks_library::Item,
    resource: Resource,
    suffix: String,
    deadline: Instant,
) -> io::Result<(&'static str, Bytes)> {
    // A disconnected request cannot interrupt an OS open on a stalled network share.
    // Bound those calls too; the opener is never held while waiting for an encoder.
    static OPENERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
    let opener = OPENERS.try_acquire().map_err(|_| io::ErrorKind::WouldBlock)?;
    let opened = tokio::task::spawn_blocking(move || {
        let _opener = opener;
        let file = super::super::files::open(&item.root, &item.path)?;
        let meta = file.metadata()?;
        if meta.len() != item.size {
            return Err(invalid("file changed since scan"));
        }
        let key = resource.encodes().then(|| rendition::FileKey::new(&item, &meta)).transpose()?;
        Ok(Opened { file, item, key })
    })
    .await??;
    let cache = state.api.hls.clone();
    if let Some(bytes) = opened.key.as_ref().and_then(|key| cache.get(key, resource)) {
        return Ok(("video/mp4", bytes));
    }
    let slots = permits(resource).await?;
    // Only admitted workers hold the catalog's keyframe array.
    let id = opened.item.id;
    let keys = state
        .db
        .call(move |conn| flicks_library::keyframes(conn, id).map_err(super::super::library_error))
        .await
        .map_err(io::Error::other)?;
    let work = Work::new(deadline);
    let _cancel = CancelOnDrop(work.cancelled.clone());
    tokio::task::spawn_blocking(move || {
        // The permits stay with a stuck OS call even if Hyper drops the request future.
        let _slots = slots;
        work.check()?;
        generate(opened, keys, resource, &suffix, &cache, &work)
    })
    .await?
}

fn generate(
    Opened { file, item, key }: Opened,
    keys: Vec<flicks_library::Keyframe>,
    resource: Resource,
    suffix: &str,
    cache: &Cache,
    work: &Work,
) -> io::Result<(&'static str, Bytes)> {
    // Another encoder may have filled this fragment while we waited.
    if let Some(bytes) = key.as_ref().and_then(|key| cache.get(key, resource)) {
        return Ok(("video/mp4", bytes));
    }
    let duration = item.duration_ms.ok_or_else(|| invalid("missing duration"))?;
    let cuts = boundaries(&keys, duration)?;
    let seq = if let Resource::Segment(_, s) = resource { s } else { 0 };
    let &[start, end] = cuts.get(seq..).and_then(|w| w.first_chunk::<2>()).ok_or(io::ErrorKind::NotFound)?;
    let mut source = match item.container.as_str() {
        "mkv" => Source::Mkv(Box::new(mkv::Source::open(file, work.clone())?)),
        "mp4" => Source::Mp4(mp4::Source::open(file, work.clone())?),
        _ => return Err(invalid("unsupported container")),
    };
    let tracks = match &mut source {
        Source::Mkv(s) => s.tracks(resource, start)?,
        Source::Mp4(s) => s.tracks()?,
    };
    let video = tracks.iter().find(|t| t.video).ok_or_else(|| invalid("no supported video"))?;
    let source_bps = item.size.saturating_mul(8000) / duration;
    let tiers = if matches!(resource.rendition(), Some(Rendition::Direct(_))) {
        Vec::new()
    } else {
        rendition::ladder(video, &cuts, source_bps)
    };
    #[cfg(target_os = "macos")]
    let tiers = if !tiers.is_empty()
        && (!video::supported(video) || tracks.iter().filter(|t| !t.video).any(|t| !stereo::supported(t)))
    {
        Vec::new()
    } else {
        tiers
    };
    let delay_ticks = u64::from(video.duration) * u64::from(video.depth);
    let delay_ticks = match &source {
        Source::Mp4(s) => delay_ticks.max(s.decode_delay(video)?),
        _ => delay_ticks,
    };
    let rescale = |ticks: u64, from: u32, to: u32| {
        u64::try_from(u128::from(ticks) * u128::from(to) / u128::from(from)).map_err(invalid)
    };
    let delay_us = rescale(delay_ticks, video.config.timescale, TS)?;
    let Some(r) = resource.rendition() else {
        return Ok((
            PLAYLIST,
            rendition::master(&tracks, video, &tiers, source_bps, item.hdr.as_deref(), suffix, delay_us).into(),
        ));
    };
    let track = tracks.iter().find(|t| t.index == r.source_index(video.index)).ok_or(io::ErrorKind::NotFound)?;
    let tier = tiers.iter().find(|t| t.name == r).copied();
    if (matches!(r, Rendition::Video(_)) && tier.is_none())
        || (matches!(r, Rendition::Stereo(_)) && (tiers.is_empty() || track.video))
    {
        return Err(io::ErrorKind::NotFound.into());
    }
    if matches!(resource, Resource::Playlist(_)) {
        return Ok((PLAYLIST, playlist(&cuts, r, suffix).into()));
    }
    if matches!(resource, Resource::Init(Rendition::Direct(_))) {
        return Ok((
            "video/mp4",
            cmaf::write_init_segment(std::slice::from_ref(&track.config)).map_err(invalid)?.into(),
        ));
    }
    #[cfg(target_os = "macos")]
    if matches!(resource, Resource::Init(Rendition::Stereo(_))) {
        return Ok(("video/mp4", stereo::init(track)?.into()));
    }
    let encoded = !matches!(r, Rendition::Direct(_));
    let delay = rescale(delay_ticks, video.config.timescale, track.config.timescale)?;
    let packets = match &mut source {
        Source::Mkv(s) => s.packets(track, start, end, encoded)?,
        Source::Mp4(s) => s.packets(track, start, end, encoded)?,
    };
    work.check()?;
    let sequence = u32::try_from(seq + 1).map_err(invalid)?;
    if !encoded {
        return Ok(("video/mp4", remux(track, &packets, sequence, delay)?.into()));
    }
    #[cfg(target_os = "macos")]
    {
        let key = key.ok_or_else(|| invalid("missing encode key"))?;
        if let Some(tier) = tier {
            let (init, media) = video::segment(track, tier, &packets, sequence, start..end, delay_us, work)?;
            let (init, media) = (Bytes::from(init), Bytes::from(media));
            if cache.get(&key, Resource::Init(r)).is_some_and(|prior| prior != init) {
                return Err(invalid("rendition configuration changed"));
            }
            cache.insert(&key, Resource::Init(r), &init);
            cache.insert(&key, Resource::Segment(r, seq), &media);
            return Ok(("video/mp4", if matches!(resource, Resource::Init(_)) { init } else { media }));
        }
        let bytes: Bytes = stereo::segment(track, &packets, seq, start, end, delay_us, work)?.into();
        cache.insert(&key, resource, &bytes);
        Ok(("video/mp4", bytes))
    }
    #[cfg(not(target_os = "macos"))]
    Err(io::ErrorKind::NotFound.into())
}

fn remux(track: &Track, packets: &[Packet], seq: u32, delay: u64) -> io::Result<Vec<u8>> {
    let first = packets.first().ok_or_else(|| invalid("empty segment"))?;
    // MP4 has native DTS. Matroska only has PTS: anchor the decode clock before
    // any CRA leading pictures and put the SPS reorder delay in each PTS instead.
    let leading =
        if track.video && first.dts.is_none() { packets.iter().filter(|p| p.pts_us < first.pts_us).count() } else { 0 };
    let start_pts = first.pts_us - leading as i64 * i64::from(track.duration);
    let base = if let Some(dts) = first.dts {
        i128::from(dts) + i128::from(delay)
    } else if track.video {
        i128::from(start_pts)
    } else {
        i128::from(first.pts_us) * i128::from(track.config.timescale) / i128::from(TS) + i128::from(delay)
    };
    let mut samples = Vec::with_capacity(packets.len());
    for (i, p) in packets.iter().enumerate() {
        let offset = match (p.dts, p.pts) {
            (Some(dts), Some(pts)) => i32::try_from(i128::from(pts) - i128::from(dts)).map_err(invalid)?,
            _ if track.video => {
                cmaf::timing::timestamp(start_pts, p.pts_us, i, track.duration, track.depth).map_err(invalid)?.1
            }
            _ => 0,
        };
        samples.push(WriteSample {
            data: &p.data,
            duration: p.duration,
            composition_offset: offset,
            is_sync: !track.video || p.keyframe,
        });
    }
    cmaf::write_media_segment(seq, u64::try_from(base).map_err(invalid)?, &samples).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flicks_container::{isobmff, matroska::Demuxer};
    use std::fs::File;

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn busy_encoders_serve_playlists_and_hits_and_cancel_on_disconnect() {
        use super::super::ENCODERS;
        use crate::server::handle;
        use http_body_util::{BodyExt, Empty};
        use hyper::{Request, StatusCode, service::service_fn};
        use std::sync::Arc;
        use tokio::io::AsyncWriteExt;

        let ffmpeg = flicks_testkit::require_ffmpeg!();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap().join("library");
        std::fs::create_dir(&root).unwrap();
        let path = root.join("episode.mp4");
        // A short first cut to cache and a long second cut, available in two real VT tiers.
        let out = std::process::Command::new(ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=1440x810:rate=25:duration=24",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-g",
                "1000",
                "-sc_threshold",
                "0",
                "-force_key_frames",
                "4",
                "-b:v",
                "6M",
                "-maxrate",
                "6M",
                "-bufsize",
                "6M",
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let state = Arc::new(State::open(&dir.path().join("data"), 0).unwrap());
        crate::api::migrate(&state).await.unwrap();
        let item = state
            .db
            .call(move |conn| {
                crate::db::add_folder(conn, root.to_str().unwrap())?;
                flicks_library::Scanner::new(None, root.join("art")).scan(conn, &root).unwrap();
                Ok(flicks_library::item(conn, 1).unwrap().unwrap())
            })
            .await
            .unwrap();
        let key = rendition::FileKey::new(&item, &path.metadata().unwrap()).unwrap();
        let sign = |uri: &str| {
            flicks_client::auth::sign(&state.key, &state.public, "GET", uri, b"", b"", crate::unix_now()).unwrap()
        };
        let get = |resource: &str| {
            let uri = format!("/hls/1/{resource}");
            let req = Request::get(&uri).header("authorization", sign(&uri)).body(Empty::<Bytes>::new()).unwrap();
            let state = state.clone();
            async move {
                let reply = handle(&state, req).await;
                assert_eq!(reply.status(), StatusCode::OK, "{uri}");
                reply.into_body().collect().await.unwrap().to_bytes()
            }
        };
        let cached = get("v480/0.m4s").await;
        let other = tokio::spawn(get("v720/1.m4s"));
        // A real HTTP/1 connection: closing the client must drop the handler, not just its body.
        let (mut client, socket) = tokio::io::duplex(4096);
        let server = state.clone();
        let connection = tokio::spawn(async move {
            hyper::server::conn::http1::Builder::new()
                .serve_connection(
                    hyper_util::rt::TokioIo::new(socket),
                    service_fn(|req| async { Ok::<_, std::convert::Infallible>(handle(&server, req).await) }),
                )
                .await
        });
        let uri = "/hls/1/v480/1.m4s";
        client
            .write_all(format!("GET {uri} HTTP/1.1\r\nHost: flicks\r\nAuthorization: {}\r\n\r\n", sign(uri)).as_bytes())
            .await
            .unwrap();
        tokio::time::timeout(DEADLINE, async {
            while ENCODERS.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let cold = Resource::parse("v480/1.m4s").unwrap();
        let queued = || permits(cold);
        tokio::select! { biased;
            _ = queued() => panic!("first waiting encode unexpectedly finished"),
            _ = queued() => panic!("second waiting encode unexpectedly finished"),
            _ = async {
                let t = Instant::now();
                assert!(get("master.m3u8").await.starts_with(b"#EXTM3U"));
                let playlist = t.elapsed();
                let t = Instant::now();
                assert_eq!(get("v480/0.m4s").await, cached);
                eprintln!("Two busy encoders: playlist {playlist:?}, cache hit {:?}", t.elapsed());
                assert!(!get("0/0.m4s").await.is_empty());
            } => {}
        }
        let t = Instant::now();
        drop(client);
        let permit = tokio::time::timeout(Duration::from_secs(1), ENCODERS.acquire()).await.unwrap().unwrap();
        assert!(!other.is_finished());
        assert!(state.api.hls.get(&key, cold).is_none(), "the disconnected encode ran to completion");
        eprintln!("Disconnect to encoder available: {:?}", t.elapsed());
        drop(permit);
        let _ = connection.await.unwrap();
        assert!(!other.await.unwrap().is_empty());
    }

    #[test]
    fn avc_without_vui_restriction_uses_level_dpb_not_sixteen() {
        use flicks_container::bitstream::BitWriter;
        let mut b = BitWriter::new();
        b.write_bits(66, 8); // Baseline, level 4, 1920x1088: at most four DPB frames.
        b.write_bits(0, 8);
        b.write_bits(40, 8);
        for _ in 0..4 {
            b.write_ue(0);
        }
        b.write_ue(4);
        b.write_bits(0, 1);
        b.write_ue(119);
        b.write_ue(67);
        b.write_bits(1, 1); // frame_mbs_only
        b.write_bits(1, 1); // direct_8x8_inference
        b.write_bits(0, 1); // no crop
        b.write_bits(1, 1); // VUI present, but no bitstream_restriction
        b.write_bits(0, 9);
        let mut sps = vec![0x67];
        sps.extend(b.bytes());
        let codec = CodecConfig::Avc(isobmff::AvcConfig::new(vec![sps], vec![vec![0x68]], 4));
        assert_eq!(sps_info(&codec).unwrap().0, 4);
    }

    #[test]
    fn sps_prefixes_and_bit_flips_use_existing_codec_parsers() {
        let (ffmpeg, _) = flicks_testkit::require_oracles!();
        let dir = tempfile::tempdir().unwrap();
        let mut count = 0;
        for encoder in ["libx264", "libx265"] {
            let path = dir.path().join(format!("{encoder}.mkv"));
            let output = std::process::Command::new(&ffmpeg)
                .args([
                    "-v",
                    "error",
                    "-y",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc2=size=160x90:rate=24",
                    "-frames:v",
                    "4",
                    "-c:v",
                    encoder,
                ])
                .arg(&path)
                .output()
                .unwrap();
            assert!(output.status.success());
            let demux = Demuxer::new(File::open(path).unwrap()).unwrap();
            let codec = mkv::config(&demux.tracks()[0], 0, None).unwrap().unwrap().config.entry.codec;
            let sps = match &codec {
                CodecConfig::Avc(c) => c.sps[0].clone(),
                CodecConfig::Hevc(c) => c.sps()[0].to_vec(),
                _ => unreachable!(),
            };
            let mut cases: Vec<_> = (0..sps.len()).map(|n| sps[..n].to_vec()).collect();
            for i in 0..sps.len() {
                for bit in 0..8 {
                    let mut n = sps.clone();
                    n[i] ^= 1 << bit;
                    cases.push(n);
                }
            }
            for n in cases {
                let mut codec = codec.clone();
                match &mut codec {
                    CodecConfig::Avc(c) => c.sps = vec![n],
                    CodecConfig::Hevc(c) => c.arrays.iter_mut().find(|a| a.nal_type == 33).unwrap().nalus = vec![n],
                    _ => unreachable!(),
                }
                if let Ok((depth, _)) = sps_info(&codec) {
                    assert!(depth <= 16);
                }
                count += 1;
            }
        }
        eprintln!("HLS SPS mutations: {count}");
    }
}
