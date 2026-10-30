//! Configuration validation uses the same SPS parsers as software decode.
use crate::{Codec, ColorInfo, Error, FrameInfo, PixelFormat, Result};
use flicks_container::{
    bitstream::{BitReader, length_prefixed_nals, unescape_rbsp},
    isobmff::{AvcConfig, HevcConfig},
};

pub(crate) struct StreamInfo {
    pub codec: Codec,
    pub length_size: usize,
    pub parameter_sets: Vec<Vec<u8>>,
    pub frame: FrameInfo,
    pub reorder: usize,
}

impl StreamInfo {
    pub fn parse(codec: Codec, data: &[u8]) -> Result<Self> {
        if data.len() > 256 * 1024 || data.first() != Some(&1) {
            return Err(Error::InvalidData("bad or oversized configuration record"));
        }
        Self::parse_inner(codec, data)
    }

    fn parse_inner(codec: Codec, data: &[u8]) -> Result<Self> {
        let invalid = || Error::InvalidData("invalid configuration or SPS");
        let (length_size, sets, width, height, crop, chroma, bits, chroma_bits, interlaced, reorder, color, sar) =
            match codec {
                Codec::H264 => {
                    let c = AvcConfig::parse(data).map_err(|_| invalid())?;
                    // Multiple SPSs may have different dimensions/reorder depths. Fall back rather
                    // than silently guessing which one is active.
                    if c.sps.len() != 1 || c.pps.is_empty() {
                        return Err(Error::Unsupported("requires one SPS and at least one PPS"));
                    }
                    let n = c.sps.first().ok_or_else(invalid)?;
                    let s = flicks_h264::params::Sps::parse(&unescape_rbsp(n.get(1..).ok_or_else(invalid)?))
                        .map_err(|_| invalid())?;
                    let v = s.vui.clone().unwrap_or_default();
                    let mut sets = c.sps;
                    sets.extend(c.pps);
                    (
                        c.length_size,
                        sets,
                        s.width(),
                        s.height(),
                        s.crop_rect(),
                        s.chroma_array_type(),
                        s.bit_depth_luma,
                        s.bit_depth_chroma,
                        !s.frame_mbs_only,
                        s.max_num_reorder_frames().min(s.max_dpb_frames()),
                        ColorInfo {
                            full_range: v.full_range,
                            primaries: v.colour_primaries,
                            transfer: v.transfer_characteristics,
                            matrix: v.matrix_coefficients,
                        },
                        v.sar,
                    )
                }
                Codec::Hevc => {
                    let c = HevcConfig::parse(data).map_err(|_| invalid())?;
                    let sets: Vec<_> = c
                        .arrays
                        .into_iter()
                        .filter(|a| (32..=34).contains(&a.nal_type))
                        .flat_map(|a| a.nalus)
                        .collect();
                    let mut sps = sets.iter().filter(|n| n.first().is_some_and(|h| (h >> 1) & 63 == 33));
                    let n = sps.next().ok_or_else(invalid)?;
                    if sps.next().is_some()
                        || !sets.iter().any(|n| n.first().is_some_and(|h| (h >> 1) & 63 == 32))
                        || !sets.iter().any(|n| n.first().is_some_and(|h| (h >> 1) & 63 == 34))
                    {
                        return Err(Error::Unsupported("requires VPS, one SPS and PPS"));
                    }
                    let s = flicks_hevc::params::Sps::parse(&unescape_rbsp(n.get(2..).ok_or_else(invalid)?))
                        .map_err(|_| invalid())?;
                    let v = s.vui.clone().unwrap_or_default();
                    (
                        c.length_size,
                        sets,
                        s.width,
                        s.height,
                        s.crop_rect(),
                        s.chroma_array_type(),
                        s.bit_depth_luma,
                        s.bit_depth_chroma,
                        v.field_seq,
                        s.max_num_reorder as usize,
                        ColorInfo {
                            full_range: v.full_range,
                            primaries: v.colour_primaries,
                            transfer: v.transfer_characteristics,
                            matrix: v.matrix_coefficients,
                        },
                        v.sar,
                    )
                }
            };
        if !matches!(length_size, 1 | 2 | 4) || sets.len() > 64 {
            return Err(invalid());
        }
        if interlaced || chroma != 1 || bits != chroma_bits || !matches!(bits, 8 | 10) || reorder > 16 {
            return Err(Error::Unsupported("requires progressive 8/10-bit 4:2:0 with reorder depth <=16"));
        }
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || crop.2 == 0
            || crop.3 == 0
            || crop.0.checked_add(crop.2).is_none_or(|x| x > width)
            || crop.1.checked_add(crop.3).is_none_or(|y| y > height)
        {
            return Err(Error::Unsupported("picture dimensions outside 1..8192"));
        }
        let info = Self {
            codec,
            length_size: length_size as usize,
            parameter_sets: sets,
            reorder,
            frame: FrameInfo {
                width,
                height,
                crop,
                pixel_format: if bits == 10 { PixelFormat::P010 } else { PixelFormat::Nv12 },
                color,
                sar,
            },
        };
        for n in &info.parameter_sets {
            let t = info.nal_type(n)?;
            if !info.is_parameter(t) {
                return Err(invalid());
            }
        }
        Ok(info)
    }

    fn nal_type(&self, n: &[u8]) -> Result<u8> {
        let h = *n.first().ok_or(Error::InvalidData("empty NAL"))?;
        if h & 128 != 0 {
            return Err(Error::InvalidData("forbidden NAL bit"));
        }
        match self.codec {
            Codec::H264 => Ok(h & 31),
            Codec::Hevc => {
                let b = *n.get(1).ok_or(Error::InvalidData("short HEVC NAL"))?;
                if b & 7 == 0 || h & 1 != 0 || b & 0xf8 != 0 {
                    return Err(Error::Unsupported("invalid temporal ID or multilayer HEVC"));
                }
                Ok((h >> 1) & 63)
            }
        }
    }

    fn is_parameter(&self, t: u8) -> bool {
        match self.codec {
            Codec::H264 => matches!(t, 7 | 8),
            Codec::Hevc => (32..=34).contains(&t),
        }
    }

    /// An open GOP starts with non-IDR I slices, not nal_unit_type 5. Check every
    /// slice so a P/B picture is still rejected as the first picture of a run.
    pub fn h264_intra(&self, data: &[u8]) -> Result<bool> {
        let invalid = || Error::InvalidData("invalid H.264 slice header");
        let nals = length_prefixed_nals(data, self.length_size).map_err(|_| invalid())?;
        let mut intra = false;
        for n in nals {
            if matches!(self.nal_type(n)?, 1 | 5) {
                let rbsp = unescape_rbsp(n.get(1..).ok_or_else(invalid)?);
                let mut r = BitReader::new(&rbsp);
                r.read_ue().map_err(|_| invalid())?; // first_mb_in_slice
                if !matches!(r.read_ue().map_err(|_| invalid())?, 2 | 7) {
                    return Ok(false);
                }
                intra = true;
            }
        }
        Ok(intra)
    }

    pub fn validate_sample(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.is_empty() || data.len() > 16 * 1024 * 1024 {
            return Err(Error::InvalidData("empty or oversized access unit"));
        }
        let mut rest = data;
        let mut types = Vec::new();
        while !rest.is_empty() {
            if types.len() >= 4096 {
                return Err(Error::InvalidData("too many NALs"));
            }
            let prefix = rest.get(..self.length_size).ok_or(Error::InvalidData("short NAL length"))?;
            let len = prefix.iter().fold(0usize, |n, &b| (n << 8) | b as usize);
            rest = rest.get(self.length_size..).ok_or(Error::InvalidData("short NAL"))?;
            let n = rest.get(..len).ok_or(Error::InvalidData("NAL length exceeds sample"))?;
            rest = rest.get(len..).ok_or(Error::InvalidData("short NAL"))?;
            let t = self.nal_type(n)?;
            if self.is_parameter(t) && !self.parameter_sets.iter().any(|p| trim_zeros(p) == trim_zeros(n)) {
                return Err(Error::Unsupported("in-band parameter change; recreate decoder"));
            }
            types.push(t);
        }
        if !types.iter().any(|&t| match self.codec {
            Codec::H264 => matches!(t, 1 | 5),
            // EOS/EOB may accompany the last picture or occupy their own sample.
            Codec::Hevc => t <= 21 || matches!(t, 36 | 37),
        }) {
            return Err(Error::InvalidData("access unit has no picture"));
        }
        Ok(types)
    }
}

fn trim_zeros(n: &[u8]) -> &[u8] {
    n.get(..n.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1)).unwrap_or_default()
}
