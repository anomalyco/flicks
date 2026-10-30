//! PTS-only Matroska timing. H.264 (H.264:2021 §7.3.2.1/E.1) and HEVC
//! (H.265:2021 §7.3.2.2) advertise the decoder's reordering bound in their SPS.
use crate::bitstream::{BitReader, unescape_rbsp};
use crate::isobmff::{CodecConfig, Error, Result};

struct Bits<'a>(BitReader<'a>);
impl Bits<'_> {
    fn bits(&mut self, n: u32) -> Result<u32> {
        self.0.read_bits(n).map_err(|_| bad())
    }
    fn flag(&mut self) -> Result<bool> {
        Ok(self.bits(1)? != 0)
    }
    fn skip(&mut self, n: usize) -> Result<()> {
        self.0.skip(n).map_err(|_| bad())
    }
    fn ue(&mut self, max: u32) -> Result<u32> {
        self.0.read_ue().map_err(|_| bad()).and_then(|v| if v <= max { Ok(v) } else { Err(bad()) })
    }
    fn se(&mut self) -> Result<i32> {
        self.0.read_se().map_err(|_| bad())
    }
}
fn bad() -> Error {
    Error::Invalid("CMAF SPS timing".into())
}

/// Maximum reorder depth, or a conservative 16-frame bound when AVC omits VUI
/// restrictions. Multiple SPSs use the largest bound. No decoder dependency.
pub fn reorder_depth(codec: &CodecConfig) -> Result<u32> {
    let mut depth = 0;
    match codec {
        CodecConfig::Avc(c) => {
            if c.sps.is_empty() || c.sps.len() > 31 {
                return Err(bad());
            }
            for sps in &c.sps {
                depth = depth.max(avc(sps)?);
            }
        }
        CodecConfig::Hevc(c) => {
            if c.arrays.len() > 32 {
                return Err(bad());
            }
            let mut count = 0;
            for a in c.arrays.iter().filter(|a| a.nal_type == 33) {
                if a.nalus.len() > 32 {
                    return Err(bad());
                }
                for sps in &a.nalus {
                    depth = depth.max(hevc(sps)?);
                    count += 1;
                }
            }
            if count == 0 {
                return Err(bad());
            }
        }
        _ => {}
    }
    Ok(depth)
}

fn rbsp(nal: &[u8], header: usize) -> Result<Vec<u8>> {
    if nal.len() > 65_535 {
        return Err(bad());
    }
    Ok(unescape_rbsp(nal.get(header..).ok_or_else(bad)?))
}

fn avc(nal: &[u8]) -> Result<u32> {
    if nal.first().is_none_or(|b| b & 0x9f != 7) {
        return Err(bad());
    }
    let data = rbsp(nal, 1)?;
    let mut r = Bits(BitReader::new(&data));
    let profile = r.bits(8)?;
    r.skip(16)?;
    r.ue(31)?;
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        let chroma = r.ue(3)?;
        if chroma == 3 {
            r.skip(1)?;
        }
        r.ue(6)?;
        r.ue(6)?;
        r.skip(1)?;
        if r.flag()? {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if r.flag()? {
                    let mut last = 8i64;
                    let mut next = 8i64;
                    for _ in 0..if i < 6 { 16 } else { 64 } {
                        if next != 0 {
                            next = (last + i64::from(r.se()?)).rem_euclid(256);
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue(12)?;
    match r.ue(2)? {
        0 => {
            r.ue(12)?;
        }
        1 => {
            r.skip(1)?;
            r.se()?;
            r.se()?;
            let n = r.ue(255)?;
            for _ in 0..n {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue(16)?;
    r.skip(1)?;
    r.ue(65_535)?;
    r.ue(65_535)?;
    if !r.flag()? {
        return Err(Error::Unsupported("interlaced CMAF".into()));
    }
    r.skip(1)?;
    if r.flag()? {
        for _ in 0..4 {
            r.ue(65_535)?;
        }
    }
    if !r.flag()? {
        return Ok(16);
    }
    if r.flag()? && r.bits(8)? == 255 {
        r.skip(32)?;
    }
    if r.flag()? {
        r.skip(1)?;
    }
    if r.flag()? {
        r.skip(4)?;
        if r.flag()? {
            r.skip(24)?;
        }
    }
    if r.flag()? {
        r.ue(5)?;
        r.ue(5)?;
    }
    if r.flag()? {
        r.skip(65)?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        hrd(&mut r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        hrd(&mut r)?;
    }
    if nal_hrd || vcl_hrd {
        r.skip(1)?;
    }
    r.skip(1)?;
    if !r.flag()? {
        return Ok(16);
    }
    r.skip(1)?;
    for _ in 0..4 {
        r.ue(32)?;
    }
    let depth = r.ue(16)?;
    if r.ue(16)? < depth {
        return Err(bad());
    }
    Ok(depth)
}

fn hrd(r: &mut Bits<'_>) -> Result<()> {
    let n = r.ue(31)?;
    r.skip(8)?;
    for _ in 0..=n {
        r.ue(u32::MAX)?;
        r.ue(u32::MAX)?;
        r.skip(1)?;
    }
    r.skip(20)
}

fn hevc(nal: &[u8]) -> Result<u32> {
    if nal.first().is_none_or(|b| (b >> 1) & 0x7f != 33) {
        return Err(bad());
    }
    let data = rbsp(nal, 2)?;
    let mut r = Bits(BitReader::new(&data));
    r.skip(4)?;
    let layers = r.bits(3)?;
    if layers > 6 {
        return Err(bad());
    }
    r.skip(1 + 96)?;
    let mut flags = Vec::new();
    for _ in 0..layers {
        flags.push((r.flag()?, r.flag()?));
    }
    if layers > 0 {
        r.skip((8 - layers) as usize * 2)?;
    }
    for (profile, level) in flags {
        if profile {
            r.skip(88)?;
        }
        if level {
            r.skip(8)?;
        }
    }
    r.ue(15)?;
    if r.ue(3)? == 3 {
        r.skip(1)?;
    }
    r.ue(65_535)?;
    r.ue(65_535)?;
    if r.flag()? {
        for _ in 0..4 {
            r.ue(65_535)?;
        }
    }
    r.ue(8)?;
    r.ue(8)?;
    r.ue(12)?;
    let all = r.flag()?;
    let mut depth = 0;
    for _ in if all { 0 } else { layers }..=layers {
        let dpb = r.ue(16)?;
        let reorder = r.ue(16)?;
        if reorder > dpb {
            return Err(bad());
        }
        depth = depth.max(reorder);
        r.ue(u32::MAX)?;
    }
    Ok(depth)
}

/// Derive a decode-order timestamp from a CFR source's keyframe PTS and frame
/// ordinal. Presentation is delayed by the SPS reorder bound to keep tfdt >= 0.
/// Apply the same `delay` to audio so lip sync is unchanged. Caller preserves
/// original PTS (including millisecond Matroska rounding), never sorts payloads.
pub fn timestamp(start_pts: i64, pts: i64, ordinal: usize, duration: u32, depth: u32) -> Result<(u64, i32)> {
    if duration == 0 || depth > 16 || ordinal > super::MAX_SAMPLES {
        return Err(bad());
    }
    let dts = i128::from(start_pts) + ordinal as i128 * i128::from(duration);
    let offset = i128::from(pts) + i128::from(depth) * i128::from(duration) - dts;
    Ok((u64::try_from(dts).map_err(|_| bad())?, i32::try_from(offset).map_err(|_| bad())?))
}
