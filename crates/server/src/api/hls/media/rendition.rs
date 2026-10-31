use super::{TS, Track};
use hyper::body::Bytes;
use std::{
    collections::VecDeque,
    fmt,
    fs::Metadata,
    io,
    sync::{Mutex, PoisonError},
    time::SystemTime,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::hls) enum Rendition {
    Direct(usize),
    Video(u16),
    Stereo(usize),
}

impl Rendition {
    pub(super) fn source_index(self, video: usize) -> usize {
        match self {
            Self::Video(_) => video,
            Self::Direct(i) | Self::Stereo(i) => i,
        }
    }
    fn parse(s: &str) -> Option<Self> {
        if let Some(s) = s.strip_prefix('v') {
            match s {
                "1080" => Some(Self::Video(1080)),
                "720" => Some(Self::Video(720)),
                "480" => Some(Self::Video(480)),
                _ => None,
            }
        } else if let Some(s) = s.strip_prefix('a') {
            s.parse().ok().map(Self::Stereo)
        } else {
            s.parse().ok().map(Self::Direct)
        }
    }
}
impl fmt::Display for Rendition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Direct(i) => write!(f, "{i}"),
            Self::Video(n) => write!(f, "v{n}"),
            Self::Stereo(i) => write!(f, "a{i}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::api::hls) enum Resource {
    Master,
    Playlist(Rendition),
    Init(Rendition),
    Segment(Rendition, usize),
}
impl Resource {
    pub(in crate::api::hls) fn parse(s: &str) -> Option<Self> {
        if s == "master.m3u8" {
            return Some(Self::Master);
        }
        if let Some(track) = s.strip_suffix(".m3u8") {
            return Some(Self::Playlist(Rendition::parse(track)?));
        }
        let (t, name) = s.split_once('/')?;
        let t = Rendition::parse(t)?;
        if name == "init.mp4" {
            Some(Self::Init(t))
        } else {
            Some(Self::Segment(t, name.strip_suffix(".m4s")?.parse().ok()?))
        }
    }
    pub(super) fn rendition(self) -> Option<Rendition> {
        match self {
            Self::Master => None,
            Self::Playlist(r) | Self::Init(r) | Self::Segment(r, _) => Some(r),
        }
    }
    pub(in crate::api::hls) fn encodes(self) -> bool {
        matches!(self, Self::Init(Rendition::Video(_)) | Self::Segment(Rendition::Video(_) | Rendition::Stereo(_), _))
    }
}

#[derive(Clone, Copy)]
pub(super) struct Tier {
    pub name: Rendition,
    pub width: u16,
    pub height: u16,
    pub bitrate: u32,
}

pub(super) fn ladder(video: &Track, cuts: &[u64], source_bps: u64) -> Vec<Tier> {
    let Some(v) = &video.config.entry.video else { return vec![] };
    if !cfg!(target_os = "macos")
        || !cuts.windows(2).all(|w| w[1] - w[0] <= 32_000)
        || !(16_666..=1_000_000)
            .contains(&(u64::from(video.duration) * u64::from(TS) / u64::from(video.config.timescale)))
        || u64::from(v.width) * u64::from(v.height) > 4096 * 2304
    {
        return vec![];
    }
    let (w, h) = (u64::from(v.width), u64::from(v.height));
    let (n, d) = v.pixel_aspect.unwrap_or((1, 1));
    if n == 0 || d == 0 {
        return vec![];
    }
    let (ratio_w, ratio_h) = (w * u64::from(n), h * u64::from(d));
    [(1920, 1080, 8_000_000), (1280, 720, 3_000_000), (854, 480, 1_200_000)]
        .into_iter()
        .filter_map(|(mw, mh, bitrate)| {
            // Never upscale, or encode an 8 Mb/s variant of a 5 Mb/s original.
            if (w <= mw && h <= mh) || u64::from(bitrate) + 192_000 >= source_bps {
                return None;
            }
            let even = |num: u64, den: u64| ((num + den) / (den * 2)) * 2;
            let (width, height) = if ratio_w * mh >= ratio_h * mw {
                (mw, even(mw * ratio_h, ratio_w))
            } else {
                (even(mh * ratio_w, ratio_h), mh)
            };
            (width >= 2 && height >= 2 && width <= w && height <= h).then_some(Tier {
                name: Rendition::Video(mh as u16),
                width: width as u16,
                height: height as u16,
                bitrate,
            })
        })
        .collect()
}

pub(super) fn master(
    tracks: &[Track],
    video: &Track,
    tiers: &[Tier],
    source_bps: u64,
    hdr: Option<&str>,
    suffix: &str,
    delay_us: u64,
) -> String {
    let audio: Vec<_> = tracks.iter().filter(|t| !t.video).collect();
    let v = video.config.entry.video.as_ref();
    let resolution = v.map(|v| format!(",RESOLUTION={}x{}", v.width, v.height)).unwrap_or_default();
    use flicks_container::isobmff::ColorInfo::{Nclc, Nclx};
    // Absent VIDEO-RANGE declares SDR in HLS. Some HDR Matroska files carry
    // transfer/primaries in the SPS only, not in their Colour element.
    let transfer = match video.source_color() {
        Some(Nclx { transfer, .. } | Nclc { transfer, .. }) if *transfer != 2 => *transfer,
        _ => u16::from(video.sps_transfer),
    };
    let range = match (transfer, hdr) {
        (16, _) | (2, Some("hdr10")) => ",VIDEO-RANGE=PQ",
        (18, _) | (2, Some("hlg")) => ",VIDEO-RANGE=HLG",
        _ => "",
    };
    // Native decoders need the original presentation timeline for seeks/progress. In
    // particular, a video-only MP4 may have a nonzero first PTS *in addition* to reorder delay.
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-SESSION-DATA:DATA-ID=\"org.flicks.presentation-delay-us\",VALUE=\"{delay_us}\"\n"
    );
    // Do not claim INDEPENDENT-SEGMENTS for direct open-GOP streams.
    for (i, t) in audio.iter().enumerate() {
        let language = &t.config.language;
        let language = if language.is_empty() || !language.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            "und"
        } else {
            language
        };
        let default = if i == 0 { "YES" } else { "NO" };
        let mut group = |id: &str, channels: u32, r: Rendition| {
            out.push_str(&format!("#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{id}\",NAME=\"Audio {}\",DEFAULT={default},AUTOSELECT=YES,CHANNELS=\"{channels}\",LANGUAGE=\"{language}\",URI=\"{r}.m3u8{suffix}\"\n",i+1))
        };
        group("audio", t.config.entry.audio.as_ref().map_or(0, |a| a.channels), Rendition::Direct(t.index));
        if !tiers.is_empty() {
            group("stereo", 2, Rendition::Stereo(t.index));
        }
    }
    let codecs = std::iter::once(video.codec.as_str())
        .chain(audio.iter().map(|t| t.codec.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",");
    let group = if audio.is_empty() { "" } else { ",AUDIO=\"audio\"" };
    let fps = f64::from(video.config.timescale) / f64::from(video.duration);
    out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={source_bps},CODECS=\"{codecs}\"{resolution},FRAME-RATE={fps:.3}{range}{group}\n{}.m3u8{suffix}\n",source_bps.saturating_mul(2),video.index));
    let (aac, group, audio_bps) =
        if audio.is_empty() { ("", "", 0) } else { (",mp4a.40.2", ",AUDIO=\"stereo\"", 192_000) };
    for t in tiers {
        let avg = t.bitrate + audio_bps;
        out.push_str(&format!("#EXT-X-STREAM-INF:BANDWIDTH={},AVERAGE-BANDWIDTH={avg},CODECS=\"avc1.64002a{aac}\",RESOLUTION={}x{},FRAME-RATE={fps:.3},VIDEO-RANGE=SDR{group}\n{}.m3u8{suffix}\n", avg*5/4+32_000,t.width,t.height,t.name));
    }
    out
}

#[derive(Clone, Eq, PartialEq)]
pub(super) struct FileKey {
    id: i64,
    oshash: String,
    size: u64,
    modified: SystemTime,
    #[cfg(target_os = "macos")]
    identity: (u64, u64, i64, i64),
}
impl FileKey {
    pub(super) fn new(item: &flicks_library::Item, meta: &Metadata) -> io::Result<Self> {
        Ok(Self {
            id: item.id,
            oshash: item.oshash.clone(),
            size: item.size,
            modified: meta.modified()?,
            #[cfg(target_os = "macos")]
            identity: {
                use std::os::unix::fs::MetadataExt;
                (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
            },
        })
    }
}

/// Per-server LRU of output bytes, never playlists or grants. Always authenticate,
/// recheck catalog visibility and open the current file before a cache lookup.
#[derive(Default)]
pub(crate) struct Cache(Mutex<VecDeque<(FileKey, Resource, Bytes)>>);
impl Cache {
    pub(super) fn get(&self, key: &FileKey, r: Resource) -> Option<Bytes> {
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let i = entries.iter().position(|(k, s, _)| k == key && *s == r)?;
        let hit = entries.remove(i)?;
        let bytes = hit.2.clone();
        entries.push_front(hit);
        Some(bytes)
    }
    #[cfg(target_os = "macos")]
    pub(super) fn insert(&self, key: &FileKey, r: Resource, bytes: &Bytes) {
        const MAX: usize = 32 << 20;
        if bytes.len() > MAX {
            return;
        }
        let mut entries = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        entries.retain(|(k, s, _)| k != key || *s != r);
        let mut total = entries.iter().map(|e| e.2.len()).sum::<usize>() + bytes.len();
        while entries.len() >= 16 || total > MAX {
            let Some(old) = entries.pop_back() else { break };
            total -= old.2.len();
        }
        entries.push_front((key.clone(), r, bytes.clone()));
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use flicks_container::isobmff::{AvcConfig, SampleEntry, TrackConfig};
    #[test]
    fn lower_only_ladder_reuses_the_source_cuts() {
        let mut track = Track {
            index: 0,
            config: TrackConfig::new(SampleEntry::avc(AvcConfig::new(vec![], vec![], 4), 3840, 2160), TS),
            duration: 41667,
            video: true,
            depth: 2,
            sps_transfer: 16,
            codec: String::new(),
            partial_color: None,
        };
        let cuts = [0, 4021, 8021, 12021];
        let dims =
            |v: &Track, rate: u64| ladder(v, &cuts, rate).iter().map(|t| (t.width, t.height)).collect::<Vec<_>>();
        assert_eq!(dims(&track, 12_000_000), [(1920, 1080), (1280, 720), (854, 480)]);
        assert_eq!(dims(&track, 3_000_000), [(854, 480)]);
        assert!(ladder(&track, &[0, 33000], 12_000_000).is_empty());
        let v = track.config.entry.video.as_mut().unwrap();
        (v.width, v.height) = (1920, 1080);
        assert_eq!(dims(&track, 12_000_000), [(1280, 720), (854, 480)]);
    }
    #[test]
    fn cached_fragments_follow_recency_and_file_revision() {
        let cache = Cache::default();
        let key =
            FileKey { id: 1, oshash: "a".into(), size: 8, modified: SystemTime::UNIX_EPOCH, identity: (1, 2, 3, 4) };
        let r = |n| Resource::Segment(Rendition::Video(720), n);
        let data = Bytes::from_static(b"sample");
        for i in 0..16 {
            cache.insert(&key, r(i), &data);
        }
        assert_eq!(cache.get(&key, r(0)), Some(data.clone()));
        cache.insert(&key, r(16), &data);
        assert!(cache.get(&key, r(1)).is_none());
        assert!(cache.get(&FileKey { identity: (2, 3, 4, 5), ..key.clone() }, r(0)).is_none());
        assert_eq!(cache.get(&key, r(0)), Some(data));
    }
}
