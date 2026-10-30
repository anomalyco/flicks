//! Signed catalog, playback and per-user progress. One file is one item.

mod files;
mod scan;

use std::path::Path;
use std::sync::Arc;

use flicks_library as library;
use hyper::body::Bytes;
use hyper::http::request::Parts;
use hyper::{Method, StatusCode};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Error;
use crate::server::{Caller, Reply, State, json, status};

pub use scan::Api;

fn library_error(error: library::Error) -> Error {
    match error {
        library::Error::Db(error) => Error::Db(error),
        other => Error::Io(std::io::Error::other(other)),
    }
}

/// Tables exist before accepting requests; slow scans run on a separate connection/thread.
pub async fn start(state: &Arc<State>) -> Result<(), Error> {
    state
        .db
        .call(|conn| {
            library::migrate(conn).map_err(library_error)?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS progress (
                user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                item_id INTEGER NOT NULL REFERENCES library_files(id) ON DELETE CASCADE,
                position INTEGER NOT NULL CHECK(position >= 0),
                played INTEGER NOT NULL,
                updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
                PRIMARY KEY (user_id, item_id)
             );",
            )?;
            Ok(())
        })
        .await?;
    scan::start(state)
}

pub fn folders_changed(state: &State) {
    state.api.wake();
}

/// `None` means an unknown route. Authentication has already checked the signature and Range.
pub async fn route(state: &State, caller: &Caller, parts: &Parts, body: &Bytes) -> Option<Reply> {
    match dispatch(state, caller, parts, body).await {
        Ok(reply) => reply,
        Err(error) => {
            eprintln!("catalog: {error}");
            Some(status(StatusCode::INTERNAL_SERVER_ERROR))
        }
    }
}

async fn dispatch(state: &State, caller: &Caller, parts: &Parts, body: &[u8]) -> Result<Option<Reply>, Error> {
    let path = parts.uri.path();
    let user = caller.user;
    if parts.method == Method::GET {
        if let Some(id) = path.strip_prefix("/files/").and_then(id) {
            return Ok(Some(files::serve(state, id, parts).await?));
        }
        if let Some(id) = path.strip_prefix("/play/").and_then(id) {
            return Ok(Some(files::play(state, user, id).await?));
        }
        if let Some(id) = path.strip_prefix("/items/").and_then(id) {
            return state
                .db
                .call(move |conn| {
                    let Some(item) = visible_item(conn, id)? else { return Ok(Some(status(StatusCode::NOT_FOUND))) };
                    Ok(Some(json(&json!({"item": item_json(&item), "streams": streams(conn, id)?,
                    "progress": progress(conn, user, id)?}))))
                })
                .await;
        }
        if let Some(id) = path.strip_prefix("/progress/").and_then(id) {
            let Some(user) = user else { return Ok(Some(status(StatusCode::FORBIDDEN))) };
            return state
                .db
                .call(move |conn| {
                    if visible_item(conn, id)?.is_none() {
                        return Ok(Some(status(StatusCode::NOT_FOUND)));
                    }
                    Ok(Some(json(&progress(conn, Some(user), id)?)))
                })
                .await;
        }
        if path == "/libraries" {
            return state
                .db
                .call(|conn| {
                    let rows: Vec<_> = crate::db::folders(conn)?
                        .into_iter()
                        .map(|f| {
                            json!({
                                "id": f.id, "name": Path::new(&f.path).file_name().unwrap_or_default().to_string_lossy()
                            })
                        })
                        .collect();
                    Ok(Some(json(&rows)))
                })
                .await;
        }
        if matches!(path, "/home" | "/items" | "/search" | "/progress") {
            let Some(query) = Query::parse(parts.uri.query()) else { return Ok(Some(status(StatusCode::BAD_REQUEST))) };
            let path = path.to_string();
            return state
                .db
                .call(move |conn| {
                    let out = match path.as_str() {
                        "/home" => json!({"continue_watching": list(conn, user, &query, "continue")?,
                        "recently_added": list(conn, user, &query, "recent")?}),
                        "/progress" => {
                            if user.is_none() {
                                return Ok(Some(status(StatusCode::FORBIDDEN)));
                            }
                            Value::Array(list(conn, user, &query, "progress")?)
                        }
                        "/search" => Value::Array(list(conn, user, &query, "search")?),
                        _ => Value::Array(list(conn, user, &query, "items")?),
                    };
                    Ok(Some(json(&out)))
                })
                .await;
        }
    }
    if parts.method == Method::PUT
        && let Some(id) = path.strip_prefix("/progress/").and_then(id)
    {
        let Some(user) = user else { return Ok(Some(status(StatusCode::FORBIDDEN))) };
        let Ok(update) = serde_json::from_slice::<Progress>(body) else {
            return Ok(Some(status(StatusCode::BAD_REQUEST)));
        };
        if update.position < 0 || update.updated_at < 0 {
            return Ok(Some(status(StatusCode::BAD_REQUEST)));
        }
        return state.db.call(move |conn| {
            if visible_item(conn, id)?.is_none() { return Ok(Some(status(StatusCode::NOT_FOUND))) }
            conn.execute(
                "INSERT INTO progress (user_id, item_id, position, played, updated_at) VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(user_id,item_id) DO UPDATE SET position=excluded.position,
                 played=excluded.played, updated_at=excluded.updated_at WHERE excluded.updated_at > progress.updated_at",
                params![user, id, update.position, update.played, update.updated_at],
            )?;
            Ok(Some(json(&progress(conn, Some(user), id)?)))
        }).await;
    }
    Ok(None)
}

fn id(text: &str) -> Option<i64> {
    text.parse().ok().filter(|id| *id > 0)
}

// No absolute paths leave the catalog API. Removed folders stop being visible immediately,
// even while a scan that started before removal is still finishing.
fn visible_item(conn: &Connection, id: i64) -> Result<Option<library::Item>, Error> {
    let active: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM library_files f JOIN folders ON folders.path=f.root WHERE f.id=?1)",
        [id],
        |r| r.get(0),
    )?;
    if !active {
        return Ok(None);
    }
    library::item(conn, id).map_err(library_error)
}

fn item_json(it: &library::Item) -> Value {
    json!({"id": it.id, "title": it.title, "year": it.year, "season": it.season,
        "episode": it.episode, "episode_end": it.episode_end, "edition": it.edition,
        "tmdb_id": it.tmdb_id, "tmdb_media": it.tmdb_media, "poster": it.poster, "backdrop": it.backdrop,
        "added_at": it.added_at, "size": it.size, "oshash": it.oshash, "container": it.container,
        "duration_ms": it.duration_ms, "video_codec": it.video_codec, "width": it.width,
        "height": it.height, "hdr": it.hdr})
}

fn streams(conn: &Connection, id: i64) -> Result<Vec<Value>, Error> {
    Ok(library::streams(conn, id).map_err(library_error)?.iter().map(|s| json!({
        "kind": match s.kind { library::StreamKind::Audio => "audio", library::StreamKind::Subtitle => "subtitle" },
        "codec": s.codec, "language": s.language, "title": s.title, "channels": s.channels,
        "default": s.default, "forced": s.forced,
    })).collect())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Progress {
    /// Milliseconds.
    position: i64,
    played: bool,
    /// Unix milliseconds, supplied by the client for offline merging.
    updated_at: i64,
}

fn progress(conn: &Connection, user: Option<i64>, id: i64) -> Result<Option<Progress>, Error> {
    Ok(conn
        .query_row(
            "SELECT position, played, updated_at FROM progress WHERE user_id=?1 AND item_id=?2",
            params![user, id],
            |r| Ok(Progress { position: r.get(0)?, played: r.get(1)?, updated_at: r.get(2)? }),
        )
        .optional()?)
}

struct Query {
    q: String,
    library: Option<i64>,
    limit: u32,
    offset: u32,
}

impl Query {
    fn parse(query: Option<&str>) -> Option<Self> {
        let mut out = Self { q: String::new(), library: None, limit: 50, offset: 0 };
        for pair in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
            let (key, value) = pair.split_once('=')?;
            match key {
                "q" => out.q = decode(value)?,
                "library" => out.library = Some(id(value)?),
                "limit" => out.limit = value.parse::<u32>().ok()?.clamp(1, 200),
                "offset" => out.offset = value.parse().ok()?,
                _ => return None,
            }
        }
        Some(out)
    }
}

fn decode(text: &str) -> Option<String> {
    let mut bytes = text.bytes();
    let mut out = Vec::with_capacity(text.len());
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'+' => b' ',
            b'%' => {
                let hi = char::from(bytes.next()?).to_digit(16)?;
                let lo = char::from(bytes.next()?).to_digit(16)?;
                (hi * 16 + lo) as u8
            }
            _ => b,
        });
    }
    String::from_utf8(out).ok()
}

fn list(conn: &Connection, user: Option<i64>, q: &Query, mode: &str) -> Result<Vec<Value>, Error> {
    let (filter, order) = match mode {
        "continue" => ("p.position > 0 AND p.played=0", "p.updated_at DESC, f.id DESC"),
        "progress" => ("p.item_id IS NOT NULL", "p.updated_at DESC, f.id DESC"),
        "search" => (
            "instr(lower(COALESCE(f.tmdb_title,f.title)),lower(?2))>0 OR instr(lower(f.title),lower(?2))>0",
            "COALESCE(f.tmdb_title,f.title), f.season, f.episode, f.id",
        ),
        "recent" => ("1", "f.added_at DESC, f.id DESC"),
        _ => ("1", "COALESCE(f.tmdb_title,f.title), f.season, f.episode, f.id"),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT f.id FROM library_files f JOIN folders ON folders.path=f.root
         LEFT JOIN progress p ON p.item_id=f.id AND p.user_id=?1
         WHERE ({filter}) AND (?3 IS NULL OR folders.id=?3) ORDER BY {order} LIMIT ?4 OFFSET ?5"
    ))?;
    let ids = stmt
        .query_map(params![user, q.q, q.library, q.limit, q.offset], |r| r.get::<_, i64>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(it) = visible_item(conn, id)? {
            let p = progress(conn, user, id)?;
            out.push(if mode == "progress" {
                json!({"item": id, "progress": p})
            } else {
                json!({"item": item_json(&it), "progress": p})
            });
        }
    }
    Ok(out)
}
