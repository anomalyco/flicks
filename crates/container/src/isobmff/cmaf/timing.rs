//! Decode-order timestamps for CFR, PTS-only sources. Codec parsers supply the
//! reorder bound; the container writer has no codec dependency.
use crate::isobmff::{Error, Result};

/// Presentation is delayed by the supplied reorder bound to keep tfdt >= 0.
/// Apply the same delay to audio. Caller preserves PTS and decode order.
pub fn timestamp(start_pts: i64, pts: i64, ordinal: usize, duration: u32, depth: u32) -> Result<(u64, i32)> {
    let bad = || Error::Invalid("CMAF timing".into());
    if duration == 0 || depth > 16 || ordinal > super::MAX_SAMPLES {
        return Err(bad());
    }
    let dts = i128::from(start_pts) + ordinal as i128 * i128::from(duration);
    let offset = i128::from(pts) + i128::from(depth) * i128::from(duration) - dts;
    Ok((u64::try_from(dts).map_err(|_| bad())?, i32::try_from(offset).map_err(|_| bad())?))
}
