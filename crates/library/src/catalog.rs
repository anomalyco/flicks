//! The catalog tables and their queries. One file is one item; a show is its episodes sharing a
//! `tmdb_id` (or a title, when unmatched).

use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use flicks_metadata::{Match, Media, Parsed};
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::Error;

/// Creates the catalog tables if they are missing. Safe to run on every start. A later change adds
/// guarded statements here; it never edits these.
pub fn migrate(conn: &Connection) -> Result<(), Error> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS library_dirs (
            path TEXT PRIMARY KEY,
            parent TEXT NOT NULL,
            hash INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS library_dirs_parent ON library_dirs(parent);
        CREATE TABLE IF NOT EXISTS library_files (
            id INTEGER PRIMARY KEY,
            path TEXT NOT NULL UNIQUE,
            dir TEXT NOT NULL,
            root TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            oshash TEXT NOT NULL,
            container TEXT NOT NULL,
            duration_ms INTEGER,
            video_codec TEXT,
            width INTEGER,
            height INTEGER,
            hdr TEXT,
            keyframes BLOB NOT NULL,
            title TEXT NOT NULL,
            year INTEGER,
            season INTEGER,
            episode INTEGER,
            episode_end INTEGER,
            edition TEXT,
            matched INTEGER NOT NULL DEFAULT 0,
            tmdb_id INTEGER,
            tmdb_media TEXT,
            tmdb_title TEXT,
            tmdb_year INTEGER,
            poster TEXT,
            backdrop TEXT,
            added_at INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS library_files_dir ON library_files(dir);
        CREATE INDEX IF NOT EXISTS library_files_root ON library_files(root);
        CREATE INDEX IF NOT EXISTS library_files_oshash ON library_files(oshash);
        CREATE INDEX IF NOT EXISTS library_files_added ON library_files(added_at);
        CREATE TABLE IF NOT EXISTS library_streams (
            file_id INTEGER NOT NULL,
            idx INTEGER NOT NULL,
            kind TEXT NOT NULL,
            codec TEXT NOT NULL,
            language TEXT NOT NULL,
            title TEXT NOT NULL,
            channels INTEGER NOT NULL,
            is_default INTEGER NOT NULL,
            forced INTEGER NOT NULL,
            PRIMARY KEY (file_id, idx)
        );
        ",
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Audio,
    Subtitle,
}

/// An audio or subtitle track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub kind: StreamKind,
    pub codec: String,
    pub language: String,
    pub title: String,
    /// Audio channels; 0 for subtitles.
    pub channels: u32,
    pub default: bool,
    pub forced: bool,
}

/// A keyframe of the first video track. `offset` is where to start reading to reach it: the
/// keyframe's own bytes in an MP4, the start of its Cluster in a Matroska file. At most one per
/// second of video.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keyframe {
    pub time_ms: u32,
    pub offset: u64,
}

/// A file in the catalog. `title`, `year` and `poster` are TMDB's when the file matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: i64,
    pub path: String,
    /// The library folder this file was found under.
    pub root: String,
    pub size: u64,
    /// OSHash, 16 hex digits.
    pub oshash: String,
    pub container: String,
    pub duration_ms: Option<u64>,
    pub video_codec: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// `hdr10` or `hlg`.
    pub hdr: Option<String>,
    pub title: String,
    pub year: Option<u16>,
    pub season: Option<u16>,
    pub episode: Option<u16>,
    pub episode_end: Option<u16>,
    pub edition: Option<String>,
    pub tmdb_id: Option<u64>,
    /// `movie` or `tv` when matched.
    pub tmdb_media: Option<String>,
    /// TMDB image paths, for `metadata::cache_artwork`.
    pub poster: Option<String>,
    pub backdrop: Option<String>,
    pub added_at: i64,
}

const ITEM: &str = "id, path, root, size, oshash, container, duration_ms, video_codec, width, height, hdr, \
    COALESCE(tmdb_title, title), COALESCE(tmdb_year, year), season, episode, episode_end, edition, \
    tmdb_id, tmdb_media, poster, backdrop, added_at";

fn item_row(r: &Row) -> rusqlite::Result<Item> {
    let n = |i: usize| r.get::<_, Option<i64>>(i);
    Ok(Item {
        id: r.get(0)?,
        path: r.get(1)?,
        root: r.get(2)?,
        size: u64::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
        oshash: r.get(4)?,
        container: r.get(5)?,
        duration_ms: n(6)?.and_then(|v| u64::try_from(v).ok()),
        video_codec: r.get(7)?,
        width: n(8)?.and_then(|v| u32::try_from(v).ok()),
        height: n(9)?.and_then(|v| u32::try_from(v).ok()),
        hdr: r.get(10)?,
        title: r.get(11)?,
        year: n(12)?.and_then(|v| u16::try_from(v).ok()),
        season: n(13)?.and_then(|v| u16::try_from(v).ok()),
        episode: n(14)?.and_then(|v| u16::try_from(v).ok()),
        episode_end: n(15)?.and_then(|v| u16::try_from(v).ok()),
        edition: r.get(16)?,
        tmdb_id: n(17)?.and_then(|v| u64::try_from(v).ok()),
        tmdb_media: r.get(18)?,
        poster: r.get(19)?,
        backdrop: r.get(20)?,
        added_at: r.get(21)?,
    })
}

pub fn item(conn: &Connection, id: i64) -> Result<Option<Item>, Error> {
    let sql = format!("SELECT {ITEM} FROM library_files WHERE id = ?1");
    Ok(conn.query_row(&sql, [id], item_row).optional()?)
}

/// Forgets files, streams and directory hashes outside the configured roots. The server's
/// progress foreign key cascades file deletion; stream deletion also covers older schemas.
pub fn retain_roots(conn: &mut Connection, roots: &[PathBuf]) -> Result<(), Error> {
    let tx = conn.transaction()?;
    let mut stmt = tx.prepare("SELECT id, root FROM library_files")?;
    let files = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut removed = Vec::new();
    for file in files {
        let (id, root) = file?;
        if !roots.iter().any(|r| r == Path::new(&root)) {
            removed.push(id);
        }
    }
    drop(stmt);
    delete_files(&tx, &removed)?;
    let dirs = {
        let mut stmt = tx.prepare("SELECT path FROM library_dirs")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?
    };
    for dir in dirs {
        if !roots.iter().any(|r| Path::new(&dir).starts_with(r)) {
            tx.execute("DELETE FROM library_dirs WHERE path = ?1", [dir])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// The audio and subtitle tracks of a file, in file order.
pub fn streams(conn: &Connection, id: i64) -> Result<Vec<Stream>, Error> {
    let mut stmt = conn.prepare(
        "SELECT kind, codec, language, title, channels, is_default, forced FROM library_streams
         WHERE file_id = ?1 ORDER BY idx",
    )?;
    let rows = stmt.query_map([id], |r| {
        Ok(Stream {
            kind: if r.get::<_, String>(0)? == "audio" { StreamKind::Audio } else { StreamKind::Subtitle },
            codec: r.get(1)?,
            language: r.get(2)?,
            title: r.get(3)?,
            channels: u32::try_from(r.get::<_, i64>(4)?).unwrap_or(0),
            default: r.get(5)?,
            forced: r.get(6)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// The keyframe index of a file, by time.
pub fn keyframes(conn: &Connection, id: i64) -> Result<Vec<Keyframe>, Error> {
    let blob: Option<Vec<u8>> =
        conn.query_row("SELECT keyframes FROM library_files WHERE id = ?1", [id], |r| r.get(0)).optional()?;
    Ok(decode_keyframes(&blob.unwrap_or_default()))
}

pub(crate) fn encode_keyframes(keys: &[Keyframe]) -> Vec<u8> {
    let mut out = Vec::with_capacity(keys.len().saturating_mul(12));
    for k in keys {
        out.extend_from_slice(&k.time_ms.to_le_bytes());
        out.extend_from_slice(&k.offset.to_le_bytes());
    }
    out
}

fn decode_keyframes(blob: &[u8]) -> Vec<Keyframe> {
    blob.as_chunks::<12>()
        .0
        .iter()
        .map(|c| {
            let mut time = [0u8; 4];
            let mut offset = [0u8; 8];
            time.copy_from_slice(&c[..4]);
            offset.copy_from_slice(&c[4..]);
            Keyframe { time_ms: u32::from_le_bytes(time), offset: u64::from_le_bytes(offset) }
        })
        .collect()
}

// Everything below is for the scanner.

/// A probed file ready to be written.
pub(crate) struct NewFile<'a> {
    pub path: &'a str,
    pub dir: &'a str,
    pub root: &'a str,
    pub mtime_ns: i64,
    pub probed: &'a crate::probe::Probed,
    pub parsed: &'a Parsed,
}

pub(crate) fn int(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

pub(crate) fn upsert(conn: &Connection, f: &NewFile, now: i64) -> Result<(), Error> {
    let p = f.probed;
    let video = p.video.as_ref();
    let id: i64 = conn.query_row(
        "INSERT INTO library_files (path, dir, root, size, mtime_ns, oshash, container, duration_ms, video_codec,
             width, height, hdr, keyframes, title, year, season, episode, episode_end, edition, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)
         ON CONFLICT(path) DO UPDATE SET
             dir = excluded.dir, root = excluded.root, size = excluded.size, mtime_ns = excluded.mtime_ns,
             oshash = excluded.oshash, container = excluded.container, duration_ms = excluded.duration_ms,
             video_codec = excluded.video_codec, width = excluded.width, height = excluded.height,
             hdr = excluded.hdr, keyframes = excluded.keyframes
         RETURNING id",
        params![
            f.path,
            f.dir,
            f.root,
            int(p.size),
            f.mtime_ns,
            format!("{:016x}", p.oshash),
            p.container,
            p.duration_ms.map(int),
            video.map(|v| v.codec.as_str()),
            video.map(|v| v.width),
            video.map(|v| v.height),
            video.and_then(|v| v.hdr),
            encode_keyframes(&p.keyframes),
            f.parsed.title,
            f.parsed.year,
            f.parsed.season,
            f.parsed.episode,
            f.parsed.episode_end,
            f.parsed.edition,
            now,
        ],
        |r| r.get(0),
    )?;
    conn.execute("DELETE FROM library_streams WHERE file_id = ?1", [id])?;
    let mut stmt = conn.prepare(
        "INSERT INTO library_streams (file_id, idx, kind, codec, language, title, channels, is_default, forced)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    for (idx, s) in (0i64..).zip(&p.streams) {
        let kind = if s.kind == StreamKind::Audio { "audio" } else { "subtitle" };
        stmt.execute(params![id, idx, kind, s.codec, s.language, s.title, s.channels, s.default, s.forced])?;
    }
    Ok(())
}

/// Known files directly in `dir`: path -> (id, size, mtime).
pub(crate) fn files_in(conn: &Connection, dir: &str) -> Result<Vec<(String, i64, i64, i64)>, Error> {
    let mut stmt = conn.prepare("SELECT path, id, size, mtime_ns FROM library_files WHERE dir = ?1")?;
    Ok(stmt.query_map([dir], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?.collect::<Result<_, _>>()?)
}

pub(crate) fn delete_files(conn: &Connection, ids: &[i64]) -> Result<(), Error> {
    for id in ids {
        conn.execute("DELETE FROM library_streams WHERE file_id = ?1", [id])?;
        conn.execute("DELETE FROM library_files WHERE id = ?1", [id])?;
    }
    Ok(())
}

pub(crate) fn dir_hash(conn: &Connection, dir: &str) -> Result<Option<i64>, Error> {
    Ok(conn.query_row("SELECT hash FROM library_dirs WHERE path = ?1", [dir], |r| r.get(0)).optional()?)
}

pub(crate) fn set_dir_hash(conn: &Connection, dir: &str, parent: &str, hash: i64) -> Result<(), Error> {
    conn.execute(
        "INSERT INTO library_dirs (path, parent, hash) VALUES (?1, ?2, ?3)
         ON CONFLICT(path) DO UPDATE SET parent = excluded.parent, hash = excluded.hash",
        params![dir, parent, hash],
    )?;
    Ok(())
}

pub(crate) fn child_dirs(conn: &Connection, dir: &str) -> Result<Vec<String>, Error> {
    let mut stmt = conn.prepare("SELECT path FROM library_dirs WHERE parent = ?1")?;
    Ok(stmt.query_map([dir], |r| r.get(0))?.collect::<Result<_, _>>()?)
}

/// Forgets `dir` and everything under it. Paths under `dir` all start with `dir` + separator, so
/// they sort between `dir/` and `dir0` and the primary-key and `dir` indexes serve the range.
pub(crate) fn remove_subtree(conn: &Connection, dir: &str) -> Result<u32, Error> {
    let lo = format!("{dir}{MAIN_SEPARATOR}");
    let hi = format!("{dir}{}", char::from(u8::try_from(MAIN_SEPARATOR).unwrap_or(b'/').saturating_add(1)));
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare("SELECT id FROM library_files WHERE dir = ?1 OR (dir >= ?2 AND dir < ?3)")?;
        stmt.query_map(params![dir, lo, hi], |r| r.get(0))?.collect::<Result<_, _>>()?
    };
    delete_files(conn, &ids)?;
    conn.execute("DELETE FROM library_dirs WHERE path = ?1 OR (path >= ?2 AND path < ?3)", params![dir, lo, hi])?;
    Ok(u32::try_from(ids.len()).unwrap_or(u32::MAX))
}

/// Files not yet looked up on TMDB, as their parsed names.
pub(crate) fn pending(conn: &Connection, root: &Path) -> Result<Vec<(i64, Parsed)>, Error> {
    let mut stmt = conn.prepare(
        "SELECT id, title, year, season, episode, episode_end, edition FROM library_files WHERE matched = 0 AND root = ?1",
    )?;
    let rows = stmt.query_map([root.to_string_lossy()], |r| {
        let n = |i: usize| r.get::<_, Option<i64>>(i).map(|v| v.and_then(|v| u16::try_from(v).ok()));
        Ok((
            r.get(0)?,
            Parsed {
                title: r.get(1)?,
                year: n(2)?,
                season: n(3)?,
                episode: n(4)?,
                episode_end: n(5)?,
                edition: r.get(6)?,
            },
        ))
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Records the outcome of a lookup: `None` means TMDB has nothing by that name.
pub(crate) fn set_match(conn: &Connection, id: i64, m: Option<&Match>) -> Result<(), Error> {
    conn.execute(
        "UPDATE library_files SET matched = 1, tmdb_id = ?2, tmdb_media = ?3, tmdb_title = ?4, tmdb_year = ?5,
             poster = ?6, backdrop = ?7 WHERE id = ?1",
        params![
            id,
            m.map(|m| int(m.id)),
            m.map(|m| if m.media == Media::Tv { "tv" } else { "movie" }),
            m.map(|m| m.title.as_str()),
            m.and_then(|m| m.year),
            m.and_then(|m| m.poster.as_deref()),
            m.and_then(|m| m.backdrop.as_deref()),
        ],
    )?;
    Ok(())
}
