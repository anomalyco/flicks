use std::cell::Cell;
use std::fs::File;
use std::io;
use std::time::Instant;

use flicks_container::{isobmff as mp4, matroska as mkv};
use image::RgbImage;
use mkv::{ByteSource, Codec};

const MAX_READ: u64 = 96 * 1024 * 1024;
const MAX_SAMPLE: usize = 16 * 1024 * 1024;

fn invalid(message: &'static str) -> io::Error {
    io::Error::other(message)
}

/// The container's existing allocation caps still apply. Additionally cap total I/O and single
/// reads; the supervising process enforces the deadline even inside a parser/decoder call.
struct Source {
    file: File,
    left: Cell<u64>,
    started: Instant,
}

impl ByteSource for Source {
    fn len(&self) -> u64 {
        ByteSource::len(&self.file)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        let left = self.left.get().checked_sub(buf.len() as u64).ok_or_else(|| invalid("read budget"))?;
        if buf.len() > MAX_SAMPLE || self.started.elapsed() >= super::DEADLINE {
            return Err(invalid("read budget"));
        }
        self.left.set(left);
        self.file.read_at(offset, buf)
    }
}

impl mp4::ByteSource for Source {
    fn len(&self) -> u64 {
        ByteSource::len(self)
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        ByteSource::read_at(self, offset, buf)
    }
}

pub(super) fn frame(file: File, times: &[u32]) -> io::Result<RgbImage> {
    let source = Source { file, left: Cell::new(MAX_READ), started: Instant::now() };
    let mut magic = [0; 4];
    source.read_at(0, &mut magic)?;
    if magic == [0x1a, 0x45, 0xdf, 0xa3] {
        let mut tried = Vec::new();
        let mut demux = mkv::Demuxer::new(source).map_err(io::Error::other)?;
        let track = demux
            .tracks()
            .iter()
            .position(|t| t.kind == mkv::TrackKind::Video && t.frames_readable())
            .ok_or_else(|| invalid("no video"))?;
        let codec = demux.tracks().get(track).ok_or_else(|| invalid("no track"))?.codec.clone();
        // Never fall back to indexing an entire file without Cues.
        if demux.file().cue_keyframes(track).is_empty() {
            return Err(invalid("no keyframes"));
        }
        for time in times {
            if tried.contains(time) {
                continue;
            }
            tried.push(*time);
            demux.seek(track, i64::from(*time) * 1_000_000).map_err(io::Error::other)?;
            for _ in 0..256 {
                let Some(packet) = demux.next_packet().map_err(io::Error::other)? else { break };
                if packet.track != track {
                    continue;
                }
                if packet.keyframe
                    && packet.data.len() <= MAX_SAMPLE
                    && let Ok(rgb) = decode(&codec, &packet.data)
                    && useful(&rgb)
                {
                    return Ok(rgb);
                }
                break;
            }
        }
    } else {
        let mut tried = Vec::new();
        let file = mp4::open(&source).map_err(io::Error::other)?;
        let (index, track) = file
            .tracks
            .iter()
            .enumerate()
            .find(|(_, t)| t.kind == mp4::TrackKind::Video)
            .ok_or_else(|| invalid("no video"))?;
        let codec = match track.codec() {
            Some(mp4::CodecConfig::Avc(c)) => Codec::Avc { avcc: c.to_bytes() },
            Some(mp4::CodecConfig::Hevc(c)) => Codec::Hevc { hvcc: c.to_bytes() },
            Some(mp4::CodecConfig::Av1(c)) => Codec::Av1 { av1c: c.to_bytes() },
            Some(mp4::CodecConfig::Vp9(_)) => Codec::Vp9 { private: Vec::new() },
            _ => return Err(invalid("unsupported video")),
        };
        for time in times {
            let ticks = i64::from(*time) * i64::from(track.timescale) / 1000;
            let Some(sample) = track.sample_at_presentation_time(ticks) else { continue };
            let key = track.sync_sample_before(sample);
            if tried.contains(&key) {
                continue;
            }
            tried.push(key);
            if track.samples.get(key).is_none_or(|s| s.size as usize > MAX_SAMPLE) {
                continue;
            }
            let data = file.read_sample(&source, index, key).map_err(io::Error::other)?;
            if let Ok(rgb) = decode(&codec, &data)
                && useful(&rgb)
            {
                return Ok(rgb);
            }
        }
    }
    Err(invalid("no representative frame"))
}

fn decode(codec: &Codec, data: &[u8]) -> io::Result<RgbImage> {
    match codec {
        Codec::Avc { avcc } => {
            let mut d = flicks_h264::Decoder::with_threads(1);
            d.configure_avcc(avcc).map_err(io::Error::other)?;
            let mut frames = d.decode(data, 0).map_err(io::Error::other)?;
            frames.extend(d.flush());
            if let Some(error) = d.take_error() {
                return Err(io::Error::other(error));
            }
            let p = frames.first().ok_or_else(|| invalid("no picture"))?;
            rgb([p.width, p.height], 8, [p.y_stride, p.uv_stride], p.color.full_range, p.color.matrix, |plane, i| {
                [p.y.as_slice(), p.u.as_slice(), p.v.as_slice()].get(plane)?.get(i).copied().map(u16::from)
            })
        }
        Codec::Hevc { hvcc } => {
            let mut d = flicks_hevc::Decoder::with_threads(1);
            d.configure_hvcc(hvcc).map_err(io::Error::other)?;
            let mut frames = d.decode(data, 0).map_err(io::Error::other)?;
            frames.extend(d.flush());
            if let Some(error) = d.take_error() {
                return Err(io::Error::other(error));
            }
            let p = frames.first().ok_or_else(|| invalid("no picture"))?;
            rgb(
                [p.width, p.height],
                p.bit_depth,
                [p.y_stride, p.uv_stride],
                p.color.full_range,
                p.color.matrix,
                |plane, i| {
                    let samples = [&p.y, &p.u, &p.v].get(plane).copied()?;
                    (i < samples.len()).then(|| samples.get(i))
                },
            )
        }
        Codec::Av1 { av1c } => {
            let mut d = flicks_av1::Decoder::with_threads(1);
            if let Some(config) = av1c.get(4..).filter(|c| !c.is_empty()) {
                d.decode(config).map_err(io::Error::other)?;
            }
            let mut frames = d.decode(data).map_err(io::Error::other)?;
            frames.extend(d.flush_result().map_err(io::Error::other)?);
            let p = frames.first().ok_or_else(|| invalid("no picture"))?;
            if p.subsampling_x != 1 || p.subsampling_y != 1 || p.mono_chrome {
                return Err(invalid("not 4:2:0"));
            }
            rgb(
                [p.width, p.height],
                u32::from(p.bit_depth),
                [p.width as usize, p.plane_width(1) as usize],
                p.full_range,
                p.matrix_coefficients,
                |plane, i| p.planes.get(plane)?.get(i).copied(),
            )
        }
        Codec::Vp9 { .. } => {
            let mut d = flicks_vp9::Decoder::with_threads(1);
            let mut frames = d.decode(data, 0).map_err(io::Error::other)?;
            frames.extend(d.flush());
            let p = frames.first().ok_or_else(|| invalid("no picture"))?;
            if !p.subsampling_x || !p.subsampling_y {
                return Err(invalid("not 4:2:0"));
            }
            let matrix = match p.color.color_space {
                1 | 3 => 6,
                5 => 9,
                _ => 1,
            };
            rgb([p.width, p.height], p.bit_depth, [p.y_stride, p.uv_stride], p.color.full_range, matrix, |plane, i| {
                let samples = [&p.y, &p.u, &p.v].get(plane).copied()?;
                (i < samples.len()).then(|| samples.get(i))
            })
        }
        _ => Err(invalid("unsupported video")),
    }
}

/// Sample only the output-sized centre crop: bounded RGB storage, 16:9 without distortion.
fn rgb(
    size: [u32; 2],
    bits: u32,
    strides: [usize; 2],
    full: bool,
    matrix: u8,
    sample: impl Fn(usize, usize) -> Option<u16>,
) -> io::Result<RgbImage> {
    let [width, height] = size;
    if width == 0 || height == 0 || width > 4096 || height > 2160 || !(8..=12).contains(&bits) {
        return Err(invalid("picture dimensions"));
    }
    let crop_w = width.min(height * 16 / 9);
    let crop_h = height.min(width * 9 / 16);
    let out_w = crop_w.min(1280) / 16 * 16;
    let out_h = out_w / 16 * 9;
    if out_h == 0 {
        return Err(invalid("tiny picture"));
    }
    let mut out = RgbImage::new(out_w, out_h);
    let factor = (1u32 << (bits - 8)) as f32;
    let (kr, kb) = match matrix {
        5 | 6 => (0.299, 0.114),
        9 | 10 => (0.2627, 0.0593),
        _ => (0.2126, 0.0722),
    };
    let kg = 1.0 - kr - kb;
    for (x, y, pixel) in out.enumerate_pixels_mut() {
        let sx = ((width - crop_w) / 2 + x * crop_w / out_w) as usize;
        let sy = ((height - crop_h) / 2 + y * crop_h / out_h) as usize;
        let yi = sy.checked_mul(strides[0]).and_then(|i| i.checked_add(sx)).ok_or_else(|| invalid("stride"))?;
        let ci =
            (sy / 2).checked_mul(strides[1]).and_then(|i| i.checked_add(sx / 2)).ok_or_else(|| invalid("stride"))?;
        let value =
            |plane, index| sample(plane, index).map(|v| v as f32 / factor).ok_or_else(|| invalid("short plane"));
        let mut luma = value(0, yi)?;
        let mut cb = value(1, ci)? - 128.0;
        let mut cr = value(2, ci)? - 128.0;
        if !full {
            luma = (luma - 16.0) * (255.0 / 219.0);
            cb *= 255.0 / 224.0;
            cr *= 255.0 / 224.0;
        }
        let r = luma + 2.0 * (1.0 - kr) * cr;
        let b = luma + 2.0 * (1.0 - kb) * cb;
        let g = (luma - kr * r - kb * b) / kg;
        *pixel = image::Rgb([r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8]);
    }
    Ok(out)
}

/// Reject black/flat images and sparse white lettering on black. Ignore outer letterbox bars.
fn useful(image: &RgbImage) -> bool {
    let (mut sum, mut square, mut lit, mut count) = (0.0, 0.0, 0, 0);
    for y in (image.height() / 8..image.height() * 7 / 8).step_by(8) {
        for x in (image.width() / 8..image.width() * 7 / 8).step_by(8) {
            let p = image.get_pixel(x, y);
            let luma = 0.2126 * f64::from(p[0]) + 0.7152 * f64::from(p[1]) + 0.0722 * f64::from(p[2]);
            sum += luma;
            square += luma * luma;
            lit += u32::from(luma > 24.0);
            count += 1;
        }
    }
    let n = f64::from(count.max(1));
    let mean = sum / n;
    mean > 18.0 && square / n - mean * mean > 100.0 && f64::from(lit) / n > 0.3
}
