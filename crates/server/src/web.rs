//! The web app's files (`flicks serve --web <dir>`, the folder `apps/web/build.sh` makes), served
//! without a signature at `/`, so the app's API requests are same-origin. Only an unsigned `GET`
//! for a file that exists in the folder is served here; anything else goes on to the API.

use std::path::{Path, PathBuf};

use hyper::header::{CACHE_CONTROL, CONTENT_TYPE, HeaderValue, X_CONTENT_TYPE_OPTIONS};
use hyper::{Response, StatusCode};

use crate::server::{Reply, full, status};

/// The largest file served (the app's wasm is a few megabytes).
const MAX_FILE: u64 = 64 * 1024 * 1024;

/// The file a request path names in `dir`: `/` is `index.html`; every segment is letters, digits,
/// `-`, `_` and `.`, not starting with a dot. Symlinks and folders aren't files.
pub fn find(dir: &Path, path: &str) -> Option<PathBuf> {
    let path = path.strip_prefix('/')?;
    let path = if path.is_empty() { "index.html" } else { path };
    if path.len() > 256 {
        return None;
    }
    let mut file = dir.to_path_buf();
    for segment in path.split('/') {
        let valid = |c: char| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.');
        if segment.is_empty() || segment.starts_with('.') || !segment.chars().all(valid) {
            return None;
        }
        file.push(segment);
    }
    let meta = std::fs::symlink_metadata(&file).ok()?;
    (meta.is_file() && meta.len() <= MAX_FILE).then_some(file)
}

/// A file from [`find`], with its content type; never cached without asking, so a rebuilt app
/// shows on the next load.
pub async fn serve(file: &Path) -> Reply {
    let Ok(bytes) = tokio::fs::read(file).await else {
        return status(StatusCode::NOT_FOUND);
    };
    let kind = match file.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };
    let mut reply = Response::new(full(bytes));
    let headers = reply.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(kind));
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    reply
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_files_inside_the_folder_are_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>").unwrap();
        std::fs::create_dir(dir.path().join("snippets")).unwrap();
        std::fs::write(dir.path().join("snippets/a.js"), "").unwrap();
        std::fs::write(dir.path().join(".secret"), "").unwrap();

        assert_eq!(find(dir.path(), "/"), Some(dir.path().join("index.html")));
        assert_eq!(find(dir.path(), "/snippets/a.js"), Some(dir.path().join("snippets/a.js")));
        for path in ["/snippets", "/snippets/", "/../index.html", "/snippets/../index.html", "/.secret", "/items", ""] {
            assert_eq!(find(dir.path(), path), None, "{path}");
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("index.html"), dir.path().join("link.html")).unwrap();
            assert_eq!(find(dir.path(), "/link.html"), None, "symlinks are refused");
        }
    }
}
