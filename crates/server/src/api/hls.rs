//! On-demand, separate-rendition VOD HLS. Only recently encoded fragments are
//! cached; no child processes or temporary files. Normal requests are device-signed.
//! Native players can request a five-minute, file/device-scoped grant with signed POST /hls/{file}/token.
mod media;
pub(super) use media::Cache;

use crate::{
    Error, keys,
    server::{Caller, Reply, State, full, status},
};
use flicks_container::isobmff::cmaf;
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use hyper::{
    Method, Response, StatusCode,
    header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue},
    http::request::Parts,
};
use serde_json::json;
use sha2::Sha256;
use std::io;

// Bound simultaneous demux/segment allocations independently of HTTP connections.
static WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);
static ENCODERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

fn location(path: &str) -> Option<(i64, &str)> {
    let (file, resource) = path.strip_prefix("/hls/")?.split_once('/')?;
    Some((super::id(file)?, resource))
}

fn message(file: i64, device: i64, expires: u64, key: &[u8]) -> Vec<u8> {
    let mut msg = b"flicks-hls-v2\0".to_vec();
    msg.extend(file.to_be_bytes());
    msg.extend(device.to_be_bytes());
    msg.extend(expires.to_be_bytes());
    msg.extend(key);
    msg
}

fn mac(state: &State, message: &[u8]) -> Result<Hmac<Sha256>, Error> {
    let mut key = [0; 32];
    Hkdf::<Sha256>::new(None, &state.key.to_bytes())
        .expand(b"flicks-hls-v2", &mut key)
        .map_err(|_| Error::KeyEncoding)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| Error::KeyEncoding)?;
    mac.update(message);
    Ok(mac)
}

fn no_referrer(mut reply: Reply) -> Reply {
    reply.headers_mut().insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    reply
}

/// The only bypass of normal request signing: an expiring server MAC over
/// this file and device. Never works on /files, /admin, or any non-GET route.
pub(crate) async fn signed_url(state: &State, parts: &Parts) -> Option<Reply> {
    let (file, _) = location(parts.uri.path())?;
    let denied = || no_referrer(status(StatusCode::UNAUTHORIZED));
    let query = match parts.uri.query() {
        Some(q) => q,
        None if parts.headers.contains_key(hyper::header::AUTHORIZATION) => return None,
        None => return Some(denied()),
    };
    let Some(token) = query.strip_prefix("token=") else {
        return Some(denied());
    };
    if parts.method != Method::GET || parts.headers.contains_key(hyper::header::AUTHORIZATION) {
        return Some(denied());
    }
    let Some((device, expires, prefix, tag)) = parse_token(token) else {
        return Some(denied());
    };
    let now = crate::unix_now();
    if expires <= now || expires > now.saturating_add(300) {
        return Some(denied());
    }
    // Authenticate bounded input before queuing any database work. Forged grants
    // are charged to the unauthenticated limiter by handle().
    if !mac(state, &message(file, device, expires, &prefix)).is_ok_and(|m| m.verify_slice(&tag).is_ok()) {
        return Some(denied());
    }
    // Removal takes effect immediately, including already issued grants.
    let owner = state
        .db
        .call(move |conn| {
            use rusqlite::OptionalExtension;
            Ok(conn
                .query_row("SELECT user_id,key FROM devices WHERE id=?1", [device], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
                })
                .optional()?)
        })
        .await;
    let (user, key) = match owner {
        Ok(Some(owner)) => owner,
        _ => return Some(denied()),
    };
    if key.get(..16) != Some(prefix.as_slice()) {
        return Some(denied());
    }
    Some(match route(state, &Caller { device: Some(device), user: Some(user), admin: false }, parts).await {
        Ok(reply) => reply,
        Err(_) => no_referrer(status(StatusCode::INTERNAL_SERVER_ERROR)),
    })
}

fn parse_token(token: &str) -> Option<(i64, u64, Vec<u8>, Vec<u8>)> {
    if token.len() > 150 {
        return None;
    }
    let mut p = token.split('.');
    let device = super::id(p.next()?)?;
    let expires = p.next()?.parse().ok()?;
    let prefix = keys::decode(p.next()?, 16)?;
    let tag = keys::decode(p.next()?, 32)?;
    if p.next().is_some() || prefix.len() != 16 || tag.len() != 32 {
        return None;
    }
    Some((device, expires, prefix, tag))
}

pub(super) async fn route(state: &State, caller: &Caller, parts: &Parts) -> Result<Reply, Error> {
    render(state, caller, parts).await.map(no_referrer)
}

async fn render(state: &State, caller: &Caller, parts: &Parts) -> Result<Reply, Error> {
    let Some((id, resource)) = location(parts.uri.path()) else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let facts = state
        .db
        .call(move |conn| {
            let Some(item) = super::visible_item(conn, id)? else {
                return Ok(None);
            };
            let keys = flicks_library::keyframes(conn, id).map_err(super::library_error)?;
            Ok(Some((item, keys)))
        })
        .await?;
    let Some((item, keys)) = facts else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    if parts.method == Method::POST && resource == "token" && parts.uri.query().is_none() {
        let Some(device) = caller.device else {
            return Ok(status(StatusCode::FORBIDDEN));
        };
        let expires = crate::unix_now().saturating_add(300);
        let key = state
            .db
            .call(move |conn| {
                Ok(conn.query_row("SELECT key FROM devices WHERE id=?1", [device], |r| r.get::<_, Vec<u8>>(0))?)
            })
            .await?;
        let prefix = key.get(..16).ok_or(Error::KeyEncoding)?;
        let tag = keys::encode(&mac(state, &message(id, device, expires, prefix))?.finalize().into_bytes());
        let prefix = keys::encode(prefix);
        return Ok(crate::server::json(
            &json!({"url": format!("/hls/{id}/master.m3u8?token={device}.{expires}.{prefix}.{tag}"), "expires": expires}),
        ));
    }
    if parts.method != Method::GET {
        return Ok(status(StatusCode::METHOD_NOT_ALLOWED));
    }
    // These are small, generated resources, not byte-range playlists. HTTP permits
    // ignoring Range with a complete 200 response (native players send bytes=0-).
    let suffix = match parts.uri.query() {
        None => String::new(),
        Some(q) if q.starts_with("token=") => format!("?{q}"),
        _ => return Ok(status(StatusCode::BAD_REQUEST)),
    };
    let Some(resource) = media::Resource::parse(resource) else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let permit = match WORKERS.try_acquire() {
        Ok(permit) => permit,
        Err(_) => return Ok(status(StatusCode::SERVICE_UNAVAILABLE)),
    };
    let deadline = std::time::Instant::now() + media::DEADLINE;
    // At most four requests are admitted, two encoding. Waiting for an encoder
    // is bounded too; return 503 rather than spawning an unbounded blocking queue.
    let encoder = if resource.encodes() {
        match tokio::time::timeout_at(deadline.into(), ENCODERS.acquire()).await {
            Ok(Ok(p)) => Some(p),
            _ => return Ok(status(StatusCode::SERVICE_UNAVAILABLE)),
        }
    } else {
        None
    };
    let cache = state.api.hls.clone();
    let work = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let _encoder = encoder;
        media::render(item, keys, resource, &suffix, &cache, deadline)
    });
    // A stuck OS codec cannot hold an HTTP connection indefinitely. Its permits
    // stay on the worker until it actually stops, not on the cancelled request.
    let Ok(joined) = tokio::time::timeout_at(deadline.into(), work).await else {
        return Ok(status(StatusCode::SERVICE_UNAVAILABLE));
    };
    let result = joined.map_err(|e| io::Error::other(e.to_string()))?;
    match result {
        Ok((mime, bytes)) => {
            let mut response = Response::new(full(bytes));
            response.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static(mime));
            response.headers_mut().insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
            Ok(response)
        }
        Err(e) => Ok(status(match e.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        })),
    }
}

fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Four seconds or the next catalogued random access point; long GOPs necessarily
/// produce longer segments. Catalog times are milliseconds, never guessed offsets.
fn boundaries(keys: &[flicks_library::Keyframe], duration: u64) -> io::Result<Vec<u64>> {
    if duration == 0 || duration > 7 * 24 * 3600 * 1000 || keys.is_empty() || keys.len() > 1_000_000 {
        return Err(invalid("missing duration/keyframes"));
    }
    let mut cuts = vec![0];
    let mut last = 0;
    let mut previous = 0;
    for key in keys {
        let time = u64::from(key.time_ms);
        if time < previous {
            return Err(invalid("unordered index"));
        }
        previous = time;
        // Keep a sub-second tail in the previous segment: video can outlast
        // audio slightly, so cutting there would advertise an empty rendition.
        if time >= last + 4000 && time.saturating_add(1000) <= duration {
            cuts.push(time);
            last = time;
        }
    }
    cuts.push(duration);
    Ok(cuts)
}

fn playlist(cuts: &[u64], track: impl std::fmt::Display, suffix: &str) -> String {
    let max = cuts.windows(2).map(|w| w[1] - w[0]).max().unwrap_or(1).div_ceil(1000);
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:{max}\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-MAP:URI=\"{track}/init.mp4{suffix}\"\n"
    );
    for (i, w) in cuts.windows(2).enumerate() {
        out.push_str(&format!("#EXTINF:{:.6},\n{track}/{i}.m4s{suffix}\n", (w[1] - w[0]) as f64 / 1000.0));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}
