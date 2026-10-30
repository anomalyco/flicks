//! The Flicks server: one HTTPS port, one SQLite database, one key file, and the `flicks` CLI
//! (docs/architecture.md, "Reaching a server", "Authentication" and "Server").

#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod api;
pub mod auth;
pub mod cli;
pub mod cli_client;
pub mod db;
pub mod folders;
pub mod pair;
pub mod server;
pub mod service;
pub mod tls;

pub use flicks_client::keys;

use std::path::PathBuf;

/// The one HTTPS port.
pub const PORT: u16 = 7686;

/// The key file in the data directory.
pub const KEY_FILE: &str = "server.key";

/// The database in the data directory.
pub const DB_FILE: &str = "flicks.db";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Client(#[from] flicks_client::Error),
    #[error("couldn't encode the server key")]
    KeyEncoding,
    #[error("TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("certificate: {0}")]
    Cert(#[from] rcgen::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("the database was written by a newer Flicks (schema {0})")]
    NewerDb(i64),
    #[error("the database task stopped")]
    DbClosed,
    #[error("HTTP: {0}")]
    Http(#[from] hyper::Error),
    #[error("can't find the home directory")]
    NoHome,
    #[error("no Flicks server is running on this machine (start one with `flicks serve`)")]
    NotRunning,
    #[error("the server answered with a malformed response")]
    BadResponse,
    #[error("{0}")]
    Usage(&'static str),
}

/// Where the server keeps its key and database: `~/Library/Application Support/Flicks` on macOS,
/// `%LOCALAPPDATA%\Flicks` on Windows, `$XDG_DATA_HOME/flicks` or `~/.local/share/flicks`
/// elsewhere.
pub fn data_dir() -> Result<PathBuf, Error> {
    if cfg!(windows) {
        let base = std::env::var_os("LOCALAPPDATA").ok_or(Error::NoHome)?;
        return Ok(PathBuf::from(base).join("Flicks"));
    }
    if !cfg!(target_os = "macos")
        && let Some(base) = std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty())
    {
        return Ok(PathBuf::from(base).join("flicks"));
    }
    let home = home_dir().ok_or(Error::NoHome)?;
    if cfg!(target_os = "macos") {
        Ok(home.join("Library/Application Support/Flicks"))
    } else {
        Ok(home.join(".local/share/flicks"))
    }
}

/// The user's home directory.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Seconds since the Unix epoch.
pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
