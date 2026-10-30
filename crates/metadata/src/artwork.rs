//! Artwork cache: each TMDB image is downloaded once and kept exactly as TMDB sent it (JPEG).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::{Error, Tmdb};

const IMAGES: &str = "https://image.tmdb.org/t/p";
/// Posters and backdrops at these sizes are well under 1 MiB; refuse anything much bigger.
const MAX_BYTES: usize = 4 << 20;
/// Makes each temp file name unique, so threads caching the same image don't share one.
static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

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
/// in `dir` and returns the JPEG file. A cached file is returned without touching the network.
pub fn cache_artwork(tmdb: &Tmdb, dir: &Path, kind: ArtworkKind, tmdb_path: &str) -> Result<PathBuf, Error> {
    let id = image_id(tmdb_path).ok_or_else(|| Error::Image(format!("bad image path {tmdb_path:?}")))?;
    let target = dir.join(format!("{}-{id}.jpg", kind.size()));
    if target.is_file() {
        return Ok(target);
    }
    let jpeg = (tmdb.fetch)(&format!("{IMAGES}/{}{tmdb_path}", kind.size()))?;
    if jpeg.len() > MAX_BYTES || !jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err(Error::Image("not a JPEG of a sane size".into()));
    }
    std::fs::create_dir_all(dir)?;
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".{}-{id}-{}-{n}.tmp", kind.size(), std::process::id()));
    std::fs::write(&tmp, &jpeg)?;
    std::fs::rename(&tmp, &target)?;
    Ok(target)
}

/// `/abc123.jpg` -> `abc123`. TMDB image names are letters and digits; anything else (a path
/// separator, `..`) is rejected so a response can't make us write outside the cache.
fn image_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix('/')?.strip_suffix(".jpg")?;
    (!id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_alphanumeric())).then_some(id)
}
