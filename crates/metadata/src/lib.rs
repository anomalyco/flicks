#![forbid(unsafe_code)]
//! Names and TMDB for the library scanner.
//!
//! - [`parse`]: title, year, season, episode and edition from a file path. Pure, never fails, needs
//!   no key and no network.
//! - [`Tmdb`]: matches a [`Parsed`] name on TMDB. Only exists when `FLICKS_TMDB_KEY` is set
//!   ([`Tmdb::from_env`]); without it everything still scans and plays under its parsed name.
//! - [`cache_artwork`]: downloads a TMDB image once and stores the JPEG as sent.
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

mod artwork;
mod parse;
mod tmdb;

pub use artwork::{ArtworkKind, cache_artwork};
pub use parse::{Parsed, normalize, parse};
pub use tmdb::{Match, Media, Tmdb};

/// Everything that can go wrong here. Callers log it and carry on with the parsed name.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http: {0}")]
    Http(String),
    #[error("tmdb response: {0}")]
    Json(String),
    #[error("image: {0}")]
    Image(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
