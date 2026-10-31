//! The signed, separate-track VOD profile emitted by Flicks, not a general URL resolver.
//! RFC 8216. Resource names are *checked*, then built locally under the same file's HLS root.
use crate::{Error, Result, Tier};

fn invalid() -> Error {
    Error::Media("invalid Flicks HLS playlist")
}

fn lines(data: &[u8], limit: usize) -> Result<impl Iterator<Item = &str>> {
    if data.len() > limit {
        return Err(invalid());
    }
    let text = std::str::from_utf8(data).map_err(|_| invalid())?;
    let mut lines = text.lines();
    if lines.next() != Some("#EXTM3U") || !lines.clone().all(|l| l.len() <= 2048 && !l.chars().any(char::is_control)) {
        return Err(invalid());
    }
    Ok(lines.filter(|l| !l.is_empty()))
}

/// Commas in quoted strings (in particular CODECS) do not separate attributes.
fn attributes(mut s: &str) -> Result<Vec<(&str, &str)>> {
    let mut out = Vec::new();
    while !s.is_empty() {
        let (key, rest) = s.split_once('=').ok_or_else(invalid)?;
        if out.len() >= 24
            || key.is_empty()
            || !key.bytes().all(|b| b.is_ascii_uppercase() || b == b'-')
            || out.iter().any(|(k, _)| *k == key)
        {
            return Err(invalid());
        }
        let (val, rest) = if let Some(q) = rest.strip_prefix('"') {
            q.split_once('"').ok_or_else(invalid)?
        } else {
            rest.split_at(rest.find(',').unwrap_or(rest.len()))
        };
        out.push((key, val));
        s = if rest.is_empty() {
            rest
        } else {
            rest.strip_prefix(',').filter(|s| !s.is_empty()).ok_or_else(invalid)?
        };
    }
    Ok(out)
}
fn attr<'a>(attrs: &[(&str, &'a str)], key: &str) -> Result<&'a str> {
    attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v).ok_or_else(invalid)
}
fn index(s: &str) -> bool {
    s.parse::<u32>().is_ok_and(|i| s == i.to_string())
}

pub(super) struct Variant {
    pub name: String,
    pub tier: Tier,
    pub bps: u64,
    pub size: [u32; 2],
}
pub(super) struct Audio {
    pub name: String,
    pub label: String,
    pub language: String,
    pub default: bool,
}
pub(super) struct Master {
    pub video: Vec<Variant>,
    pub audio: Vec<Audio>,
    pub origin_ns: Option<i64>,
}

pub(super) fn master(bytes: &[u8]) -> Result<Master> {
    let mut video = Vec::<Variant>::new();
    let mut audio = Vec::<Audio>::new();
    let mut stereo = false;
    let mut origin_ns = None;
    let mut lines = lines(bytes, 64 << 10)?;
    while let Some(line) = lines.next() {
        if let Some(inf) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            let attrs = attributes(inf)?;
            let name = lines.next().and_then(|l| l.strip_suffix(".m3u8")).ok_or_else(invalid)?;
            let tier = match name {
                "v480" => Tier::P480,
                "v720" => Tier::P720,
                "v1080" => Tier::P1080,
                s if index(s) => Tier::Direct,
                _ => return Err(invalid()),
            };
            let rate = attr(&attrs, "AVERAGE-BANDWIDTH").or_else(|_| attr(&attrs, "BANDWIDTH"))?;
            let bps = rate.parse().ok().filter(|b| (1..=10_000_000_000u64).contains(b)).ok_or_else(invalid)?;
            let (w, h) = attr(&attrs, "RESOLUTION")?.split_once('x').ok_or_else(invalid)?;
            let dim = |s: &str| s.parse::<u16>().ok().filter(|n| *n > 0).map(u32::from).ok_or_else(invalid);
            let size = [dim(w)?, dim(h)?];
            if video.iter().any(|v| v.tier == tier || v.size == size) {
                return Err(invalid());
            }
            if tier != Tier::Direct
                && let Ok(group) = attr(&attrs, "AUDIO")
            {
                if group != "stereo" {
                    return Err(invalid());
                }
                stereo = true;
            }
            video.push(Variant { name: name.into(), tier, bps, size });
        } else if let Some(data) = line.strip_prefix("#EXT-X-SESSION-DATA:") {
            let attrs = attributes(data)?;
            if attr(&attrs, "DATA-ID")? == "org.flicks.presentation-delay-us" {
                let us =
                    attr(&attrs, "VALUE")?.parse::<u32>().ok().filter(|&us| us <= 17_000_000).ok_or_else(invalid)?;
                if origin_ns.replace(i64::from(us) * 1000).is_some() {
                    return Err(invalid());
                }
            }
        } else if let Some(media) = line.strip_prefix("#EXT-X-MEDIA:") {
            let attrs = attributes(media)?;
            if attr(&attrs, "TYPE")? != "AUDIO" {
                return Err(invalid());
            }
            let uri = attr(&attrs, "URI")?.strip_suffix(".m3u8").ok_or_else(invalid)?;
            match attr(&attrs, "GROUP-ID")? {
                "audio" if index(uri) => continue,
                "stereo" if uri.strip_prefix('a').is_some_and(index) => (),
                _ => return Err(invalid()),
            }
            if audio.len() == 32 || audio.iter().any(|a| a.name == uri) {
                return Err(invalid());
            }
            audio.push(Audio {
                name: uri.into(),
                label: attr(&attrs, "NAME")?.into(),
                language: attr(&attrs, "LANGUAGE")?.into(),
                default: attr(&attrs, "DEFAULT")? == "YES",
            });
        } else if !line.starts_with('#') || (line.starts_with("#EXT") && line != "#EXT-X-VERSION:7") {
            return Err(invalid());
        }
    }
    video.sort_by_key(|v| (v.bps, v.tier == Tier::Direct));
    if video.last().is_none_or(|v| v.tier != Tier::Direct) || stereo == audio.is_empty() {
        return Err(invalid());
    }
    Ok(Master { video, audio, origin_ns })
}

/// Millisecond cut points. EXT-X-MAP and every segment URI must identify this same track;
/// no redirects, keys, discontinuities, byte ranges, live windows or unscoped signed GETs.
pub(super) fn media(bytes: &[u8], name: &str) -> Result<Vec<u32>> {
    let mut cuts = vec![0u32];
    let (mut map, mut ended) = (false, false);
    let mut lines = lines(bytes, 8 << 20)?;
    while let Some(line) = lines.next() {
        if ended {
            return Err(invalid());
        }
        if let Some(s) = line.strip_prefix("#EXTINF:") {
            let s = s.strip_suffix(',').ok_or_else(invalid)?;
            let seconds = s.parse::<f64>().map_err(|_| invalid())?;
            if !s.bytes().all(|b| b.is_ascii_digit() || b == b'.') || !(0.001..=32.0).contains(&seconds) || !map {
                return Err(invalid());
            }
            let end = cuts
                .last()
                .copied()
                .ok_or_else(invalid)?
                .checked_add((seconds * 1000.0).round() as u32)
                .filter(|n| *n <= 7 * 24 * 3600 * 1000)
                .ok_or_else(invalid)?;
            if cuts.len() > 151_200 || lines.next() != Some(&format!("{name}/{}.m4s", cuts.len() - 1)) {
                return Err(invalid());
            }
            cuts.push(end);
        } else if let Some(s) = line.strip_prefix("#EXT-X-MAP:") {
            if map || s != format!("URI=\"{name}/init.mp4\"") {
                return Err(invalid());
            }
            map = true;
        } else if line == "#EXT-X-ENDLIST" {
            ended = true;
        } else if matches!(line, "#EXT-X-VERSION:7" | "#EXT-X-PLAYLIST-TYPE:VOD" | "#EXT-X-MEDIA-SEQUENCE:0")
            || line
                .strip_prefix("#EXT-X-TARGETDURATION:")
                .is_some_and(|s| s.parse::<u32>().is_ok_and(|n| (1..=32).contains(&n)))
            || (line.starts_with('#') && !line.starts_with("#EXT"))
        {
        } else {
            return Err(invalid());
        }
    }
    if !ended || cuts.len() < 2 || !cuts.windows(2).take(cuts.len() - 2).all(|w| w[1] - w[0] >= 4000) {
        return Err(invalid());
    }
    Ok(cuts)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=12000000,AVERAGE-BANDWIDTH=9000000,CODECS=\"avc1.64002a,mp4a.40.2\",RESOLUTION=3840x2160\n0.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=854x480\nv480.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:5\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"v480/init.mp4\"\n#EXTINF:4.001000,\nv480/0.m4s\n#EXTINF:2.500000,\nv480/1.m4s\n#EXT-X-ENDLIST\n";
    #[test]
    fn vod_uris_are_scoped_and_cuts_are_bounded() {
        let m = master(MASTER.as_bytes()).unwrap();
        let delay = "#EXT-X-SESSION-DATA:DATA-ID=\"org.flicks.presentation-delay-us\",VALUE=\"83333\"\n";
        assert_eq!(master(format!("{MASTER}{delay}").as_bytes()).unwrap().origin_ns, Some(83_333_000));
        assert!(master(format!("{MASTER}{delay}{delay}").as_bytes()).is_err());
        assert!(master(format!("{MASTER}{}", delay.replace("83333", "4294967295")).as_bytes()).is_err());
        assert_eq!(
            m.video.iter().map(|v| (v.tier, v.bps)).collect::<Vec<_>>(),
            [(Tier::P480, 1_500_000), (Tier::Direct, 9_000_000)]
        );
        assert_eq!(media(MEDIA.as_bytes(), "v480").unwrap(), [0, 4001, 6501]);
        for bad in
            ["//other/0.m3u8", "https://other/a", "../0.m3u8", "0.m3u8?token=x", "%2e%2e/0.m3u8", "0.m3u8\rGET /admin"]
        {
            assert!(master(MASTER.replace("0.m3u8\n#EXT", &format!("{bad}\n#EXT")).as_bytes()).is_err());
        }
        for (old, new) in [
            ("init.mp4", "../init.mp4"),
            ("v480/1.m4s", "v480/0.m4s"),
            ("4.001000", "NaN"),
            ("4.001000", "-1"),
            ("4.001000", "33"),
            ("4.001000", "0.001"),
            ("#EXT-X-ENDLIST", ""),
            ("#EXT-X-ENDLIST", "#EXT-X-DISCONTINUITY\n#EXT-X-ENDLIST"),
            ("VOD", "EVENT"),
        ] {
            assert!(media(MEDIA.replace(old, new).as_bytes(), "v480").is_err());
        }
        for original in [MASTER.as_bytes(), MEDIA.as_bytes()] {
            for n in 0..original.len() {
                let _ = master(&original[..n]);
                let _ = media(&original[..n], "v480");
            }
            for b in 0..original.len() * 8 {
                let mut bytes = original.to_vec();
                bytes[b / 8] ^= 1 << (b % 8);
                if let Ok(m) = master(&bytes) {
                    assert!(m.video.len() <= 4 && m.video.last().unwrap().tier == Tier::Direct);
                }
                if let Ok(cuts) = media(&bytes, "v480") {
                    assert!(cuts.windows(2).all(|w| w[0] < w[1] && w[1] - w[0] <= 32_000));
                }
            }
        }
    }
}
