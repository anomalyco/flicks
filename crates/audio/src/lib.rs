#![forbid(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]
//! Audio for the Flicks player: one canonical channel layout across decoders, the ITU-R BS.775
//! stereo downmix, resampling to the device rate, and device output (macOS).
//!
//! Decoders hand over planar f32 in their own channel order; [`layout`] names each order as a
//! list of [`Channel`]s. [`Output::push`] takes interleaved frames with that list, a rate and a
//! timestamp, remixes them to the device ([`mix::matrix`]), resamples ([`Resampler`]) and queues
//! them; [`Output::position`] reports the timestamp being heard. Nothing here compresses dynamic
//! range. Of the decoders only `flicks-ac3` applies any (`dynrng`, always, with no switch yet).

pub mod layout;
pub mod mix;
#[cfg(target_os = "macos")]
mod output;
mod resample;

pub use layout::Channel;
#[cfg(target_os = "macos")]
pub use output::Output;
pub use resample::Resampler;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no output device")]
    NoDevice,
    #[error("audio device: {0}")]
    Device(String),
    #[error("unsupported audio format: {0}")]
    Format(&'static str),
}

impl Error {
    #[cfg(target_os = "macos")]
    fn device(e: impl std::fmt::Display) -> Error {
        Error::Device(e.to_string())
    }
}

/// Interleaves planar channels (a decoder's output) into `out`, up to the shortest channel.
pub fn interleave(planar: &[Vec<f32>], out: &mut Vec<f32>) {
    let frames = planar.iter().map(Vec::len).min().unwrap_or(0);
    out.reserve(frames * planar.len());
    for i in 0..frames {
        out.extend(planar.iter().filter_map(|c| c.get(i)));
    }
}
