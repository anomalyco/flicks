//! The walk: list a directory, hash its listing, and only look further when the hash changed.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use flicks_metadata::{ArtworkKind, Match, Tmdb, cache_artwork, normalize, parse};
use rusqlite::Connection;

use crate::catalog::{self, NewFile};
use crate::{Error, probe};

/// Deeper than any real library; bounds the recursion.
const MAX_DEPTH: usize = 64;
const EXTENSIONS: &[&str] = &["mkv", "webm", "mp4", "m4v", "mov"];

/// What a scan did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// Directories listed.
    pub listed: u32,
    /// Media files opened: every one of them was probed and hashed.
    pub opened: u32,
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    /// Files that could not be probed. They are not in the catalog, and their directory is looked at
    /// again on the next scan.
    pub failed: u32,
}

pub struct Scanner {
    tmdb: Option<Tmdb>,
    artwork_dir: PathBuf,
}

impl Scanner {
    /// `tmdb` is `Tmdb::from_env()` or `None`; matched artwork is cached in `artwork_dir`.
    pub fn new(tmdb: Option<Tmdb>, artwork_dir: PathBuf) -> Self {
        Self { tmdb, artwork_dir }
    }

    /// Scans the whole library folder `root`: adds and updates files, forgets removed ones.
    pub fn scan(&self, conn: &mut Connection, root: &Path) -> Result<ScanReport, Error> {
        let root = fs::canonicalize(root)?;
        let mut report = ScanReport::default();
        self.walk(conn, &root, &root, 0, &mut report)?;
        self.match_pending(conn, &[root])?;
        Ok(report)
    }

    /// Scans only `dirs` (what [`crate::Watcher`] reports) and what is under them, each within the
    /// library folder in `roots` that holds it. Others are ignored.
    pub fn scan_changed(
        &self,
        conn: &mut Connection,
        roots: &[PathBuf],
        dirs: &[PathBuf],
    ) -> Result<ScanReport, Error> {
        let roots = roots.iter().map(fs::canonicalize).collect::<Result<Vec<_>, _>>()?;
        let mut report = ScanReport::default();
        for dir in dirs {
            let Some(root) = roots.iter().filter(|r| dir.starts_with(r)).max_by_key(|r| r.components().count()) else {
                continue;
            };
            self.walk(conn, root, dir, 0, &mut report)?;
        }
        self.match_pending(conn, &roots)?;
        Ok(report)
    }

    fn walk(
        &self,
        conn: &mut Connection,
        root: &Path,
        dir: &Path,
        depth: usize,
        report: &mut ScanReport,
    ) -> Result<(), Error> {
        let Some(dir_s) = dir.to_str() else { return Ok(()) };
        if depth > MAX_DEPTH {
            return Ok(());
        }
        // A watcher can report a directory that was replaced with a link since the last scan.
        for part in dir.ancestors().take_while(|p| *p != root) {
            if fs::symlink_metadata(part).is_ok_and(|m| m.file_type().is_symlink()) {
                report.removed += catalog::remove_subtree(conn, dir_s)?;
                return Ok(());
            }
        }
        let entries = match list(dir) {
            Ok(entries) => entries,
            // The library folder itself failing means an unmounted drive or a wrong path, not an
            // empty library: leave the catalog alone.
            Err(e) if dir == root => return Err(e.into()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                report.removed += catalog::remove_subtree(conn, dir_s)?;
                return Ok(());
            }
            Err(_) => return Ok(()),
        };
        report.listed += 1;
        let hash = listing_hash(&entries);
        if catalog::dir_hash(conn, dir_s)? != Some(hash) {
            let parent = dir.parent().and_then(Path::to_str).unwrap_or("");
            let complete = self.sync_dir(conn, root, dir_s, &entries, report)?;
            let live: HashSet<&str> = entries.iter().filter(|e| e.is_dir).filter_map(|e| e.path.to_str()).collect();
            for gone in catalog::child_dirs(conn, dir_s)?.iter().filter(|c| !live.contains(c.as_str())) {
                report.removed += catalog::remove_subtree(conn, gone)?;
            }
            if complete {
                catalog::set_dir_hash(conn, dir_s, parent, hash)?;
            }
        }
        for sub in entries.iter().filter(|e| e.is_dir) {
            self.walk(conn, root, &sub.path, depth + 1, report)?;
        }
        Ok(())
    }

    /// Brings the catalog's files of `dir` in line with its listing. Returns false when a file
    /// could not be probed.
    fn sync_dir(
        &self,
        conn: &mut Connection,
        root: &Path,
        dir: &str,
        entries: &[Entry],
        report: &mut ScanReport,
    ) -> Result<bool, Error> {
        let known: HashMap<String, (i64, i64, i64)> = catalog::files_in(conn, dir)?
            .into_iter()
            .map(|(path, id, size, mtime)| (path, (id, size, mtime)))
            .collect();
        let root_s = root.to_string_lossy();
        let now = unix_now();
        let mut probed = Vec::new();
        let mut complete = true;
        for e in entries.iter().filter(|e| !e.is_dir) {
            let Some(path) = e.path.to_str() else { continue };
            if known.get(path).is_some_and(|&(_, size, mtime)| size == catalog::int(e.size) && mtime == e.mtime_ns) {
                continue;
            }
            // Resolved again here: the link could have been repointed since the listing.
            let Some(real) = resolve(root, &e.path) else { continue };
            let Ok(file) = File::open(&real) else {
                report.failed += 1;
                complete = false;
                continue;
            };
            report.opened += 1;
            match probe::probe(&file) {
                Ok(p) => {
                    let rel = e.path.strip_prefix(root).unwrap_or(&e.path);
                    let rel: Vec<&str> = rel.components().filter_map(|c| c.as_os_str().to_str()).collect();
                    probed.push((path, e.mtime_ns, p, parse(&rel.join("/"))));
                }
                Err(_) => {
                    report.failed += 1;
                    complete = false;
                }
            }
        }
        let listed: HashSet<&str> = entries.iter().filter(|e| !e.is_dir).filter_map(|e| e.path.to_str()).collect();
        let gone: Vec<i64> =
            known.iter().filter(|(path, _)| !listed.contains(path.as_str())).map(|(_, v)| v.0).collect();
        let tx = conn.transaction()?;
        catalog::delete_files(&tx, &gone)?;
        report.removed += u32::try_from(gone.len()).unwrap_or(u32::MAX);
        for (path, mtime_ns, p, parsed) in &probed {
            catalog::upsert(&tx, &NewFile { path, dir, root: &root_s, mtime_ns: *mtime_ns, probed: p, parsed }, now)?;
            if known.contains_key(*path) {
                report.updated += 1;
            } else {
                report.added += 1;
            }
        }
        tx.commit()?;
        Ok(complete)
    }

    /// Looks up files that haven't been tried on TMDB. A failed request stops the pass: the files
    /// stay unmatched and the next scan tries again. Without a client nothing happens.
    fn match_pending(&self, conn: &mut Connection, roots: &[PathBuf]) -> Result<(), Error> {
        let Some(tmdb) = &self.tmdb else { return Ok(()) };
        let mut pending = Vec::new();
        for root in roots {
            pending.extend(catalog::pending(conn, root)?);
        }
        let mut found: HashMap<(String, Option<u16>, bool), Option<Match>> = HashMap::new();
        let mut done = Vec::new();
        for (id, parsed) in pending {
            let key = (normalize(&parsed.title), parsed.year, parsed.is_episode());
            if !found.contains_key(&key) {
                let Ok(m) = tmdb.find(&parsed) else { break };
                if let Some(m) = &m {
                    self.cache_art(tmdb, m);
                }
                found.insert(key.clone(), m);
            }
            let m = found.get(&key).cloned().flatten();
            let still = match (&m, parsed.season, parsed.episode) {
                (Some(m), Some(season), Some(episode)) => {
                    let Ok(still) = tmdb.episode_still(m.id, season, episode) else { break };
                    still
                }
                (Some(m), _, _) => m.backdrop.clone(),
                _ => None,
            };
            if let Some(path) = &still
                && cache_artwork(tmdb, &self.artwork_dir, ArtworkKind::Backdrop, path).is_err()
            {
                break;
            }
            done.push((id, m, still));
        }
        let tx = conn.transaction()?;
        for (id, m, still) in &done {
            catalog::set_match(&tx, *id, m.as_ref(), still.as_deref())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Best effort: unavailable artwork does not prevent cataloging a match.
    fn cache_art(&self, tmdb: &Tmdb, m: &Match) {
        if let Some(p) = &m.poster {
            let _ = cache_artwork(tmdb, &self.artwork_dir, ArtworkKind::Poster, p);
        }
        if let Some(p) = &m.backdrop {
            let _ = cache_artwork(tmdb, &self.artwork_dir, ArtworkKind::Backdrop, p);
        }
    }
}

/// `path` with every link followed, if it is inside the library folder `root`. A catalog path goes
/// through this each time it is opened, not only at scan time.
pub fn resolve(root: &Path, path: &Path) -> Option<PathBuf> {
    let real = fs::canonicalize(path).ok()?;
    real.starts_with(fs::canonicalize(root).ok()?).then_some(real)
}

struct Entry {
    name: String,
    path: PathBuf,
    is_dir: bool,
    size: u64,
    mtime_ns: i64,
}

/// The videos and subfolders of `dir`, by name. Hidden names and all symlinks are skipped.
fn list(dir: &Path) -> io::Result<Vec<Entry>> {
    let mut out = Vec::new();
    for ent in fs::read_dir(dir)? {
        let Ok(ent) = ent else { continue };
        let Ok(name) = ent.file_name().into_string() else { continue };
        let Ok(kind) = ent.file_type() else { continue };
        if name.starts_with('.') || kind.is_symlink() {
            continue;
        }
        let path = ent.path();
        if kind.is_dir() {
            out.push(Entry { name, path, is_dir: true, size: 0, mtime_ns: 0 });
            continue;
        }
        let meta = if kind.is_file() {
            let Ok(m) = ent.metadata() else { continue };
            m
        } else {
            continue;
        };
        let video = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)));
        if !video {
            continue;
        }
        let mtime_ns = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .and_then(|d| i64::try_from(d.as_nanos()).ok())
            .unwrap_or(0);
        out.push(Entry { name, path, is_dir: false, size: meta.len(), mtime_ns });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// FNV-1a over names, kinds, sizes and mtimes, as the database's signed integer.
fn listing_hash(entries: &[Entry]) -> i64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut write = |bytes: &[u8]| {
        for &b in bytes {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    };
    for e in entries {
        write(e.name.as_bytes());
        write(&[0, u8::from(e.is_dir)]);
        write(&e.size.to_le_bytes());
        write(&e.mtime_ns.to_le_bytes());
    }
    i64::from_ne_bytes(h.to_ne_bytes())
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).ok().and_then(|d| i64::try_from(d.as_secs()).ok()).unwrap_or(0)
}
