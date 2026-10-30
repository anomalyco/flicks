#![forbid(unsafe_code)]
//! The library scanner and catalog.
//!
//! - [`Scanner::scan`] walks a folder: each directory is listed once and its listing (names, sizes
//!   and mtimes of videos and subfolders) hashed; a directory whose hash is unchanged costs that one
//!   listing and nothing else, no media file is opened. New and changed files are probed with
//!   `container` (codecs, duration, resolution, HDR, audio and subtitle tracks, keyframe index from
//!   Cues or sample tables) and identified by OSHash.
//! - [`Watcher`] reports the directories that changed under the library folders;
//!   [`Scanner::scan_changed`] rescans just those.
//! - Names come from `metadata`; with a TMDB client the files are matched too, without one they
//!   keep their parsed names ([`Scanner::new`]).
//! - The catalog lives in the tables [`migrate`] creates on a connection the caller hands in; this
//!   crate has no database of its own and never touches `user_version`. Reads and probes happen
//!   outside transactions and each directory is one short write, so scan on a connection of its own.
//!
//! All symlinks are skipped, both files and folders.
//! [`resolve`] is the same check for a catalog path at the moment it is opened to be served.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unimplemented,
        clippy::todo,
        clippy::unreachable
    )
)]

mod catalog;
mod probe;
mod scan;
#[cfg(test)]
mod tests;
mod watch;

pub use catalog::{Item, Keyframe, Stream, StreamKind, item, keyframes, migrate, retain_roots, streams};
pub use scan::{ScanReport, Scanner, resolve};
pub use watch::Watcher;

/// Everything that can go wrong. A file that can't be probed is counted in
/// [`ScanReport::failed`], not an error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("watch: {0}")]
    Watch(#[from] notify::Error),
}
