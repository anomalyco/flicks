use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Component, Path};
use std::pin::Pin;
use std::task::{Context, Poll};

use flicks_container::{isobmff::boxes::BoxHeader, matroska};
use http_body_util::BodyExt;
use hyper::body::{Body, Bytes, Frame, SizeHint};
use hyper::header::{ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, HeaderValue, RANGE};
use hyper::http::request::Parts;
use hyper::{Response, StatusCode};
use serde::Serialize;
use serde_json::json;
use tokio::io::{AsyncRead, AsyncSeekExt, ReadBuf};

use super::{item_json, library_error, progress, streams, visible_item};
use crate::{
    Error,
    server::{Reply, State, json, status},
};

const CHUNK: usize = 64 * 1024;

/// Inclusive byte offsets, usable verbatim in `Range: bytes=start-end`.
#[derive(Debug, Clone, Copy, Serialize)]
struct Range {
    start: u64,
    end: u64,
}

fn number(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn range(header: &str, size: u64) -> Option<Range> {
    let (start, end) = header.strip_prefix("bytes=")?.split_once('-')?;
    if size == 0 {
        return None;
    }
    if start.is_empty() {
        let suffix = number(end)?;
        return (suffix > 0).then_some(Range { start: size.saturating_sub(suffix), end: size - 1 });
    }
    let start = number(start)?;
    let end = if end.is_empty() { size - 1 } else { number(end)?.min(size - 1) };
    (start < size && start <= end).then_some(Range { start, end })
}

/// Recheck containment on every open, and refuse symlinks (including directory components).
fn open(root: &str, path: &str) -> io::Result<File> {
    let denied = || io::Error::from(io::ErrorKind::PermissionDenied);
    let root = Path::new(root);
    let path = Path::new(path);
    let real = flicks_library::resolve(root, path).ok_or_else(denied)?;
    let relative = path.strip_prefix(root).map_err(|_| denied())?;
    if relative.components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(denied());
    }
    for part in path.ancestors() {
        if std::fs::symlink_metadata(part)?.file_type().is_symlink() {
            return Err(denied());
        }
        if part == root {
            break;
        }
    }
    let file = File::open(real)?;
    if !file.metadata()?.is_file() {
        return Err(denied());
    }
    Ok(file)
}

pub(super) async fn artwork(state: &State, name: &str) -> Result<Reply, Error> {
    let Some(id) =
        name.strip_prefix("w342-").or_else(|| name.strip_prefix("w780-")).and_then(|s| s.strip_suffix(".jpg"))
    else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    if !super::image_id(id) {
        return Ok(status(StatusCode::BAD_REQUEST));
    }
    let root = state.data_dir.join("artwork");
    let path = root.join(name);
    let opened = tokio::task::spawn_blocking(move || open(&root.to_string_lossy(), &path.to_string_lossy()))
        .await
        .map_err(|e| io::Error::other(e.to_string()))?;
    let file = match opened {
        Ok(file) => file,
        Err(error) => return Ok(io_status(error)),
    };
    let len = file.metadata()?.len();
    let mut reply = Response::new(FileBody { file: tokio::fs::File::from_std(file), remaining: len }.boxed());
    insert(&mut reply, CONTENT_LENGTH, len.to_string())?;
    reply.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("image/jpeg"));
    Ok(reply)
}

fn io_status(error: io::Error) -> Reply {
    status(match error.kind() {
        io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
        io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof => StatusCode::UNPROCESSABLE_ENTITY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    })
}

pub(super) async fn serve(state: &State, id: i64, parts: &Parts) -> Result<Reply, Error> {
    let Some(item) = state.db.call(move |conn| visible_item(conn, id)).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let opened = tokio::task::spawn_blocking(move || {
        let file = open(&item.root, &item.path)?;
        let size = file.metadata()?.len();
        Ok::<_, io::Error>((file, size))
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?;
    let (file, size) = match opened {
        Ok(opened) => opened,
        Err(error) => return Ok(io_status(error)),
    };
    let requested = parts.headers.get(RANGE);
    let selected = match requested {
        Some(header) => match header.to_str().ok().and_then(|h| range(h, size)) {
            Some(range) => Some(range),
            None => {
                let mut reply = status(StatusCode::RANGE_NOT_SATISFIABLE);
                insert(&mut reply, CONTENT_RANGE, format!("bytes */{size}"))?;
                reply.headers_mut().insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
                return Ok(reply);
            }
        },
        None => None,
    };
    let (start, len) = selected.map_or((0, size), |r| (r.start, r.end - r.start + 1));
    let mut file = tokio::fs::File::from_std(file);
    if start != 0 {
        file.seek(SeekFrom::Start(start)).await?;
    }
    let mut reply = Response::new(FileBody { file, remaining: len }.boxed());
    if let Some(r) = selected {
        *reply.status_mut() = StatusCode::PARTIAL_CONTENT;
        insert(&mut reply, CONTENT_RANGE, format!("bytes {}-{}/{size}", r.start, r.end))?;
    }
    insert(&mut reply, CONTENT_LENGTH, len.to_string())?;
    reply.headers_mut().insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    reply.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    Ok(reply)
}

fn insert(reply: &mut Reply, key: hyper::header::HeaderName, value: String) -> Result<(), Error> {
    reply.headers_mut().insert(key, HeaderValue::from_str(&value).map_err(|_| Error::BadResponse)?);
    Ok(())
}

/// At most 64 KiB per frame; Hyper's backpressure drives disk reads. Dropping the body closes it.
struct FileBody {
    file: tokio::fs::File,
    remaining: u64,
}

impl Body for FileBody {
    type Data = Bytes;
    type Error = io::Error;

    fn poll_frame(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Result<Frame<Bytes>, io::Error>>> {
        if self.remaining == 0 {
            return Poll::Ready(None);
        }
        let mut bytes = vec![0; self.remaining.min(CHUNK as u64) as usize];
        let mut buf = ReadBuf::new(&mut bytes);
        match Pin::new(&mut self.file).poll_read(cx, &mut buf) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(error)) => {
                self.remaining = 0;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len();
                if n == 0 {
                    self.remaining = 0;
                    return Poll::Ready(Some(Err(io::ErrorKind::UnexpectedEof.into())));
                }
                self.remaining -= n as u64;
                bytes.truncate(n);
                Poll::Ready(Some(Ok(Frame::data(Bytes::from(bytes)))))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.remaining == 0
    }
    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.remaining)
    }
}

pub(super) async fn play(state: &State, user: Option<i64>, id: i64) -> Result<Reply, Error> {
    let facts = state
        .db
        .call(move |conn| {
            let Some(item) = visible_item(conn, id)? else { return Ok(None) };
            Ok(Some((
                item,
                streams(conn, id)?,
                flicks_library::keyframes(conn, id).map_err(library_error)?,
                progress(conn, user, id)?,
            )))
        })
        .await?;
    let Some((item, streams, keys, progress)) = facts else { return Ok(status(StatusCode::NOT_FOUND)) };
    let opened = tokio::task::spawn_blocking(move || {
        let mut file = open(&item.root, &item.path)?;
        let size = file.metadata()?.len();
        if size != item.size {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "file changed since scan"));
        }
        let header = headers(&mut file, &item.container, size)?;
        Ok::<_, io::Error>((item, size, header))
    })
    .await
    .map_err(|e| io::Error::other(e.to_string()))?;
    let (item, size, header) = match opened {
        Ok(opened) => opened,
        Err(error) => return Ok(io_status(error)),
    };
    let position = progress.as_ref().filter(|p| !p.played).map_or(0, |p| p.position as u64);
    let resume = keys.iter().rev().find(|k| u64::from(k.time_ms) <= position && k.offset < size);
    let offset = resume.map_or(0, |k| k.offset);
    let media = (size > 0).then(|| Range { start: offset, end: offset.saturating_add(1024 * 1024 - 1).min(size - 1) });
    let keys: Vec<_> = keys.iter().map(|k| json!({"time_ms": k.time_ms, "offset": k.offset})).collect();
    Ok(json(&json!({"item": item_json(&item), "streams": streams, "keyframes": keys,
        "progress": progress, "url": format!("/files/{id}"),
        "ranges": {"header": header, "media": media}, "resume_ms": position})))
}

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid media headers")
}

fn read_header(file: &mut File, offset: u64, size: u64) -> io::Result<Vec<u8>> {
    let len = size.checked_sub(offset).ok_or_else(invalid)?.min(32) as usize;
    let mut bytes = vec![0; len];
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// Locate actual metadata, not an arbitrary prefix: MP4's moov may be at EOF; MKV's
/// SeekHead may put Tracks/Info/Cues after the first Cluster. No media payload is read.
fn headers(file: &mut File, container: &str, size: u64) -> io::Result<Vec<Range>> {
    let mut ranges = Vec::new();
    if container == "mp4" {
        let mut offset = 0;
        let mut moov = false;
        while size.saturating_sub(offset) >= 8 {
            let h = BoxHeader::parse(&read_header(file, offset, size)?).map_err(|_| invalid())?;
            let len = h.size.unwrap_or(size - offset);
            let end = offset.saturating_add(len).min(size);
            let full = h.kind == b"moov" || h.kind == b"ftyp";
            moov |= h.kind == b"moov";
            ranges.push(Range { start: offset, end: if full { end - 1 } else { offset + h.header_len - 1 } });
            offset = end;
        }
        if !moov {
            return Err(invalid());
        }
    } else {
        let doc = matroska::open(&*file).map_err(|_| invalid())?;
        let end = doc.first_cluster.unwrap_or(size);
        if end > 0 {
            ranges.push(Range { start: 0, end: end - 1 });
        }
        for entry in &doc.seek_head {
            use matroska::element_ids::{CHAPTERS, CUES, INFO, SEEK_HEAD, TRACKS};
            if !matches!(entry.id, INFO | TRACKS | CUES | SEEK_HEAD | CHAPTERS) {
                continue;
            }
            let Some(start) = doc.segment_data_start.checked_add(entry.position) else { continue };
            let Ok(bytes) = read_header(file, start, size) else { continue };
            let Some((id, il)) = matroska::ebml::read_id(&bytes) else { continue };
            let Some(tail) = bytes.get(il..) else { continue };
            let Some((matroska::ebml::Size::Known(len), sl)) = matroska::ebml::read_size(tail) else {
                continue;
            };
            if id != entry.id {
                continue;
            }
            let Some(end) = start.checked_add((il + sl) as u64).and_then(|n| n.checked_add(len)).filter(|&n| n <= size)
            else {
                continue;
            };
            ranges.push(Range { start, end: end - 1 });
        }
    }
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range> = Vec::new();
    for r in ranges {
        if let Some(last) = merged.last_mut()
            && r.start <= last.end.saturating_add(1)
        {
            last.end = last.end.max(r.end);
        } else {
            merged.push(r);
        }
    }
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn symlinks_above_the_root_do_not_block_serving() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real/library");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("movie"), b"media").unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(dir.path().join("real"), &alias).unwrap();
        let root = alias.join("library");
        assert!(open(root.to_str().unwrap(), root.join("movie").to_str().unwrap()).is_ok());
    }

    #[test]
    fn mp4_header_mutations_stay_in_bounds() {
        use std::io::Write;
        let original = b"\0\0\0\x08moov\0\0\0\x10freepadding!";
        let mut cases: Vec<_> = (0..=original.len()).map(|len| original[..len].to_vec()).collect();
        for i in 0..original.len() {
            for bit in 0..8 {
                let mut bytes = original.to_vec();
                bytes[i] ^= 1 << bit;
                cases.push(bytes);
            }
        }
        let mut file = tempfile::tempfile().unwrap();
        for bytes in cases {
            file.set_len(0).unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&bytes).unwrap();
            if let Ok(ranges) = headers(&mut file, "mp4", bytes.len() as u64) {
                assert!(!ranges.is_empty());
                for r in ranges {
                    assert!(r.start <= r.end && r.end < bytes.len() as u64);
                }
            }
        }
    }

    #[test]
    fn range_mutations_stay_in_bounds() {
        let valid = b"bytes=12-34";
        let mut cases = Vec::new();
        for len in 0..valid.len() {
            cases.push(valid[..len].to_vec());
        }
        for i in 0..valid.len() {
            for bit in 0..8 {
                let mut b = valid.to_vec();
                b[i] ^= 1 << bit;
                cases.push(b);
            }
        }
        for bytes in cases {
            if let Ok(text) = std::str::from_utf8(&bytes) {
                for size in [0, 1, 20, 100, u64::MAX] {
                    if let Some(r) = range(text, size) {
                        assert!(r.start <= r.end && r.end < size);
                    }
                }
            }
        }
    }
}
