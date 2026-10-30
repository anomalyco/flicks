//! The web app's files (`flicks serve --web <dir>`, the folder `apps/web/build.sh` makes), served
//! without a signature at `/`, so the app's API requests are same-origin. Only an unsigned `GET`
//! for a file that exists in the folder is served here; anything else goes on to the API.

use std::path::{Path, PathBuf};

use hyper::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HeaderValue, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use hyper::{Response, StatusCode};

use crate::Error;
use crate::server::{Reply, full, status};

/// What the app may load: its own files, its wasm, blob/data images, same-origin requests, no framing.
const POLICY: &str = "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self'; \
    img-src 'self' blob: data:; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'";

/// The folder given to `--web`, resolved: not a protected location (the data folder holds the
/// server's key and database), and holding the built app.
pub fn check(dir: &Path, data_dir: &Path) -> Result<PathBuf, Error> {
    let dir =
        crate::folders::check(dir, data_dir).map_err(|e| Error::Io(std::io::Error::other(format!("--web: {e}"))))?;
    for path in ["/", "/app_bg.wasm"] {
        if find(&dir, path).is_none() {
            return Err(Error::Io(std::io::Error::other(format!(
                "--web: {} has no index.html and app_bg.wasm (build it with apps/web/build.sh)",
                dir.display()
            ))));
        }
    }
    Ok(dir)
}

/// Whether `meta` is a plain file or folder, not a symlink (or, on Windows, a junction or other reparse point).
fn plain(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    if std::os::windows::fs::MetadataExt::file_attributes(meta) & 0x400 != 0 {
        return false;
    }
    !meta.file_type().is_symlink()
}

/// The largest file served (the app's wasm is a few megabytes).
const MAX_FILE: u64 = 64 * 1024 * 1024;

/// The file a request path names in `dir`: `/` is `index.html`; every segment is letters, digits,
/// `-`, `_` and `.`, not starting with a dot. Symlinks (at any level) and folders aren't files.
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
        if !plain(&std::fs::symlink_metadata(&file).ok()?) {
            return None;
        }
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
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    headers.insert(CONTENT_SECURITY_POLICY, HeaderValue::from_static(POLICY));
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
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.js"), "").unwrap();
            std::os::unix::fs::symlink(outside.path(), dir.path().join("out")).unwrap();
            assert_eq!(find(dir.path(), "/out/secret.js"), None, "symlinked folders are refused");
        }
    }

    #[test]
    fn the_web_folder_must_hold_the_app_and_not_the_data_folder() {
        let data = tempfile::tempdir().unwrap();
        std::fs::write(data.path().join("index.html"), "").unwrap();
        std::fs::write(data.path().join("app_bg.wasm"), "").unwrap();
        assert!(check(data.path(), data.path()).is_err(), "the data folder is refused");

        let web = tempfile::tempdir().unwrap();
        assert!(check(web.path(), data.path()).is_err(), "an empty folder is refused");
        std::fs::write(web.path().join("index.html"), "").unwrap();
        std::fs::write(web.path().join("app_bg.wasm"), "").unwrap();
        assert!(check(web.path(), data.path()).is_ok());
    }

    #[tokio::test]
    async fn responses_carry_the_security_headers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "<!doctype html>").unwrap();
        let reply = serve(&dir.path().join("index.html")).await;
        let header = |name| reply.headers().get(name).and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
        assert!(header("content-security-policy").contains("frame-ancestors 'none'"));
        assert_eq!(header("x-content-type-options"), "nosniff");
        assert_eq!(header("referrer-policy"), "no-referrer");
    }
}
