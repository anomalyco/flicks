//! Artwork cache: each TMDB image is downloaded once and kept as a lossy WebP.

use std::io::Cursor;
use std::path::{Path, PathBuf};

use crate::{Error, Tmdb};

const IMAGES: &str = "https://image.tmdb.org/t/p";
const WEBP_QUALITY: f32 = 80.0;
/// Posters and backdrops are at most 780 x 1200 at these sizes; refuse anything much bigger.
const MAX_PIXELS: u32 = 4096 * 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtworkKind {
    /// 342 px wide.
    Poster,
    /// 780 px wide.
    Backdrop,
}

impl ArtworkKind {
    fn size(self) -> &'static str {
        match self {
            Self::Poster => "w342",
            Self::Backdrop => "w780",
        }
    }
}

/// Makes sure `tmdb_path` (a `Match::poster` or `Match::backdrop`, like `/abc123.jpg`) is cached
/// in `dir` and returns the WebP file. A cached file is returned without touching the network.
pub fn cache_artwork(tmdb: &Tmdb, dir: &Path, kind: ArtworkKind, tmdb_path: &str) -> Result<PathBuf, Error> {
    let id = image_id(tmdb_path).ok_or_else(|| Error::Image(format!("bad image path {tmdb_path:?}")))?;
    let target = dir.join(format!("{}-{id}.webp", kind.size()));
    if target.is_file() {
        return Ok(target);
    }
    let jpeg = (tmdb.fetch)(&format!("{IMAGES}/{}{tmdb_path}", kind.size()))?;
    let webp = to_webp(&jpeg)?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}-{id}.tmp", kind.size()));
    std::fs::write(&tmp, &webp)?;
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

/// `/abc123.jpg` -> `abc123`. TMDB image names are letters and digits; anything else (a path
/// separator, `..`) is rejected so a response can't make us write outside the cache.
fn image_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix('/')?.strip_suffix(".jpg")?;
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}

fn to_webp(jpeg: &[u8]) -> Result<Vec<u8>, Error> {
    let mut reader = image::ImageReader::new(Cursor::new(jpeg));
    reader.set_format(image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_PIXELS / 1024);
    limits.max_image_height = Some(MAX_PIXELS / 1024);
    limits.max_alloc = Some(u64::from(MAX_PIXELS) * 4);
    reader.limits(limits);
    let rgb = reader.decode().map_err(|e| Error::Image(e.to_string()))?.to_rgb8();
    let encoded = webp::Encoder::from_rgb(rgb.as_raw(), rgb.width(), rgb.height()).encode(WEBP_QUALITY);
    Ok(encoded.to_vec())
}
