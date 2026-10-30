use crate::{Error, Result};
use flicks_container::matroska::Codec;
use flicks_gpu::{Color, CpuFrame, Matrix, PixelFormat, Primaries, Range, Transfer};

/// PTS is in seconds. A hardware buffer can be handed directly to `Renderer::import`.
/// Padded/cropped hardware pictures are copied to a cropped CPU frame, since the renderer
/// currently samples the whole texture. No coded padding is displayed.
pub enum VideoFrame {
    Hardware(flicks_platform::Frame),
    Software(CpuVideo),
}

impl VideoFrame {
    pub fn pts(&self) -> f64 {
        match self {
            Self::Hardware(f) => f.pts as f64 / 1e9,
            Self::Software(f) => f.pts,
        }
    }
    pub fn color(&self) -> Color {
        match self {
            Self::Hardware(f) => {
                color(f.info.color.full_range, f.info.color.primaries, f.info.color.transfer, f.info.color.matrix)
            }
            Self::Software(f) => f.color,
        }
    }
    pub fn size(&self) -> [u32; 2] {
        match self {
            Self::Hardware(f) => [f.info.crop.2, f.info.crop.3],
            Self::Software(f) => f.size,
        }
    }
}

pub struct CpuVideo {
    pub pts: f64,
    pub size: [u32; 2],
    pub color: Color,
    pub format: PixelFormat,
    planes: [Vec<u8>; 2],
}

impl CpuVideo {
    pub fn as_frame(&self) -> CpuFrame<'_> {
        let stride = self.size[0] * if self.format == PixelFormat::P010 { 2 } else { 1 };
        CpuFrame {
            size: self.size,
            format: self.format,
            planes: [&self.planes[0], &self.planes[1]],
            strides: [stride; 2],
            color: self.color,
        }
    }
}

/// The single decode seam: each backend returns presentation-ordered, owned frames.
pub(crate) enum Decoder {
    Hardware(flicks_platform::Decoder),
    H264(Box<flicks_h264::Decoder>),
    Hevc(Box<flicks_hevc::Decoder>),
    Av1(Box<flicks_av1::Decoder>),
    Vp9(Box<flicks_vp9::Decoder>),
}

impl Decoder {
    pub fn new(codec: &Codec, software: bool) -> Result<Self> {
        if !software {
            let hardware = match codec {
                Codec::Avc { avcc } => Some((flicks_platform::Codec::H264, avcc)),
                Codec::Hevc { hvcc } => Some((flicks_platform::Codec::Hevc, hvcc)),
                _ => None,
            };
            if let Some((codec, config)) = hardware
                && let Ok(d) = flicks_platform::Decoder::new(codec, config)
            {
                return Ok(Self::Hardware(d));
            }
        }
        Ok(match codec {
            Codec::Avc { avcc } => Self::H264(Box::new(flicks_h264::Decoder::from_avcc(avcc).map_err(Error::decode)?)),
            Codec::Hevc { hvcc } => Self::Hevc(Box::new(flicks_hevc::Decoder::from_hvcc(hvcc).map_err(Error::decode)?)),
            Codec::Av1 { av1c } => {
                let mut d = flicks_av1::Decoder::new();
                if let Some(obus) = av1c.get(4..).filter(|b| !b.is_empty()) {
                    d.decode(obus).map_err(Error::decode)?;
                }
                Self::Av1(Box::new(d))
            }
            Codec::Vp9 { .. } => Self::Vp9(Box::default()),
            _ => return Err(Error::Media("unsupported video codec")),
        })
    }

    pub fn hardware(&self) -> bool {
        matches!(self, Self::Hardware(_))
    }

    pub fn decode(&mut self, data: Option<(&[u8], i64)>) -> Result<Vec<VideoFrame>> {
        match self {
            Self::Hardware(d) => {
                let frames = match data {
                    Some((b, p)) => d.decode(b, p),
                    None => d.flush(),
                }
                .map_err(Error::decode)?;
                frames.into_iter().map(hardware_frame).collect()
            }
            Self::H264(d) => {
                let frames = match data {
                    Some((b, p)) => d.decode(b, p).map_err(Error::decode)?,
                    None => d.flush(),
                };
                if let Some(e) = d.take_error() {
                    return Err(Error::decode(e));
                }
                frames
                    .into_iter()
                    .map(|p| {
                        planar(
                            p.pts,
                            [p.width, p.height],
                            8,
                            [p.y_stride, p.uv_stride],
                            color(p.color.full_range, p.color.primaries, p.color.transfer, p.color.matrix),
                            |plane, i| {
                                [p.y.as_slice(), p.u.as_slice(), p.v.as_slice()]
                                    .get(plane)?
                                    .get(i)
                                    .copied()
                                    .map(u16::from)
                            },
                        )
                    })
                    .collect()
            }
            Self::Hevc(d) => {
                let frames = match data {
                    Some((b, p)) => d.decode(b, p).map_err(Error::decode)?,
                    None => d.flush(),
                };
                if let Some(e) = d.take_error() {
                    return Err(Error::decode(e));
                }
                frames
                    .into_iter()
                    .map(|p| {
                        planar(
                            p.pts,
                            [p.width, p.height],
                            p.bit_depth,
                            [p.y_stride, p.uv_stride],
                            color(p.color.full_range, p.color.primaries, p.color.transfer, p.color.matrix),
                            |plane, i| {
                                let v = [&p.y, &p.u, &p.v].get(plane).copied()?;
                                (i < v.len()).then(|| v.get(i))
                            },
                        )
                    })
                    .collect()
            }
            Self::Av1(d) => {
                let frames = match data {
                    Some((b, p)) => d.decode_pts(b, p),
                    None => d.flush_result(),
                }
                .map_err(Error::decode)?;
                frames
                    .into_iter()
                    .map(|p| {
                        if p.subsampling_x != 1 || p.subsampling_y != 1 || p.mono_chrome {
                            return Err(Error::Media("only 4:2:0 video supported"));
                        }
                        planar(
                            p.pts,
                            [p.width, p.height],
                            u32::from(p.bit_depth),
                            [p.width as usize, p.plane_width(1) as usize],
                            color(p.full_range, p.color_primaries, p.transfer_characteristics, p.matrix_coefficients),
                            |plane, i| p.planes.get(plane)?.get(i).copied(),
                        )
                    })
                    .collect()
            }
            Self::Vp9(d) => {
                let frames = match data {
                    Some((b, p)) => d.decode(b, p).map_err(Error::decode)?,
                    None => d.flush(),
                };
                frames
                    .into_iter()
                    .map(|p| {
                        if !p.subsampling_x || !p.subsampling_y {
                            return Err(Error::Media("only 4:2:0 video supported"));
                        }
                        planar(
                            p.pts,
                            [p.width, p.height],
                            p.bit_depth,
                            [p.y_stride, p.uv_stride],
                            color(
                                p.color.full_range,
                                if p.color.color_space == 5 { 9 } else { 1 },
                                if p.color.color_space == 7 { 13 } else { 1 },
                                match p.color.color_space {
                                    1 | 3 => 6,
                                    5 => 9,
                                    _ => 1,
                                },
                            ),
                            |plane, i| {
                                let v = [&p.y, &p.u, &p.v].get(plane).copied()?;
                                (i < v.len()).then(|| v.get(i))
                            },
                        )
                    })
                    .collect()
            }
        }
    }
}

fn planar(
    pts: i64,
    size: [u32; 2],
    bits: u32,
    strides: [usize; 2],
    color: Color,
    sample: impl Fn(usize, usize) -> Option<u16>,
) -> Result<VideoFrame> {
    let [w, h] = size;
    if w == 0 || h == 0 || w > 8192 || h > 8192 || w % 2 != 0 || h % 2 != 0 || !matches!(bits, 8 | 10) {
        return Err(Error::Media("unsupported video dimensions or depth"));
    }
    let bytes = if bits == 8 { 1 } else { 2 };
    let mut planes =
        [Vec::with_capacity(w as usize * h as usize * bytes), Vec::with_capacity(w as usize * h as usize * bytes / 2)];
    for (plane, out) in planes.iter_mut().enumerate() {
        let rows = if plane == 0 { h } else { h / 2 };
        for y in 0..rows as usize {
            for x in 0..w as usize {
                let (p, i) = if plane == 0 { (0, y * strides[0] + x) } else { (1 + x % 2, y * strides[1] + x / 2) };
                let value = sample(p, i).ok_or(Error::Media("short decoded plane"))?;
                if bits == 8 {
                    out.push(value as u8);
                } else {
                    out.extend_from_slice(&(value << 6).to_le_bytes());
                }
            }
        }
    }
    Ok(VideoFrame::Software(CpuVideo {
        pts: pts as f64 / 1e9,
        size,
        color,
        format: if bits == 8 { PixelFormat::Nv12 } else { PixelFormat::P010 },
        planes,
    }))
}

fn hardware_frame(f: flicks_platform::Frame) -> Result<VideoFrame> {
    let info = f.info;
    if info.crop == (0, 0, info.width, info.height) {
        return Ok(VideoFrame::Hardware(f));
    }
    let planes = f.copy_planes().map_err(Error::decode)?;
    let bits = if info.pixel_format == flicks_platform::PixelFormat::Nv12 { 8 } else { 10 };
    let (x, y, w, h) = info.crop;
    planar(
        f.pts,
        [w, h],
        bits,
        [w as usize, w as usize / 2],
        color(info.color.full_range, info.color.primaries, info.color.transfer, info.color.matrix),
        |p, i| {
            let index = if p == 0 {
                (i / w as usize + y as usize) * info.width as usize + i % w as usize + x as usize
            } else {
                (i / (w as usize / 2) + y as usize / 2) * info.width as usize
                    + (i % (w as usize / 2) + x as usize / 2) * 2
                    + p
                    - 1
            };
            let plane = planes.get(usize::from(p != 0))?;
            if bits == 8 {
                plane.get(index).copied().map(u16::from)
            } else {
                Some(u16::from_le_bytes([*plane.get(index * 2)?, *plane.get(index * 2 + 1)?]) >> 6)
            }
        },
    )
}

fn color(full: bool, primaries: u8, transfer: u8, matrix: u8) -> Color {
    Color {
        matrix: match matrix {
            5 | 6 => Matrix::Bt601,
            9 => Matrix::Bt2020,
            _ => Matrix::Bt709,
        },
        range: if full { Range::Full } else { Range::Limited },
        transfer: match transfer {
            16 => Transfer::Pq,
            18 => Transfer::Hlg,
            13 => Transfer::Srgb,
            _ => Transfer::Bt1886,
        },
        primaries: if primaries == 9 { Primaries::Bt2020 } else { Primaries::Bt709 },
        peak_nits: 1000.0,
    }
}
