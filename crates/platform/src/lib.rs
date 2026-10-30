//! Hardware H.264 / HEVC decode. Input is one length-prefixed access unit in decode order;
//! output is retained, IOSurface-backed NV12/P010 in presentation order, without CPU copies.
//! Unsupported streams and hardware failures are errors: restart from a random-access point
//! with the software decoder. A seek likewise creates a fresh decoder. No FFmpeg is linked.
#![deny(unsafe_code)]
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

#[cfg(any(target_os = "macos", test))]
mod config;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod ffi;

#[cfg(windows)]
#[allow(unsafe_code)]
#[path = "permissions_ffi.rs"]
pub mod permissions;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported hardware stream: {0}")]
    Unsupported(&'static str),
    #[error("invalid video data: {0}")]
    InvalidData(&'static str),
    #[error("{operation} failed ({status})")]
    Platform { operation: &'static str, status: i32 },
    #[error("decoder is finished or failed; create a new decoder")]
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    /// 10 significant bits in the high bits of little-endian 16-bit words.
    P010,
}

/// H.273 code points from the SPS VUI. Unspecified values remain 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColorInfo {
    pub full_range: bool,
    pub primaries: u8,
    pub transfer: u8,
    pub matrix: u8,
}

#[derive(Clone, Copy, Debug)]
pub struct FrameInfo {
    /// Buffer dimensions (may include coded padding).
    pub width: u32,
    pub height: u32,
    /// Visible rectangle: x, y, width, height in luma samples.
    pub crop: (u32, u32, u32, u32),
    pub pixel_format: PixelFormat,
    pub color: ColorInfo,
    /// Sample aspect ratio; (0, 0) means unspecified.
    pub sar: (u16, u16),
}

/// Owns a decoded buffer independently of the decoder's lifetime.
pub struct Frame {
    pub pts: i64,
    pub info: FrameInfo,
    #[cfg(target_os = "macos")]
    buffer: ffi::Buffer,
}

impl Frame {
    /// Borrow for zero-copy GPU import. The GPU must retain its imported resources until done.
    #[cfg(target_os = "macos")]
    pub fn pixel_buffer(&self) -> &objc2_core_video::CVPixelBuffer {
        self.buffer.pixel_buffer()
    }

    /// Copy the full buffer's two planes without row padding (Y, interleaved CbCr).
    /// For diagnostics / software consumers only: normal playback imports the IOSurface.
    pub fn copy_planes(&self) -> Result<[Vec<u8>; 2]> {
        #[cfg(target_os = "macos")]
        {
            self.buffer.copy_planes()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(Error::Unsupported("VideoToolbox requires macOS"))
        }
    }
}

/// One stream/run, with bounded asynchronous decode and SPS-sized presentation reordering.
pub struct Decoder {
    #[cfg(target_os = "macos")]
    session: ffi::Session,
    #[cfg(target_os = "macos")]
    info: config::StreamInfo,
    #[cfg(target_os = "macos")]
    pending: Vec<Frame>,
    #[cfg(target_os = "macos")]
    in_flight: Vec<i64>,
    #[cfg(target_os = "macos")]
    last_pts: Option<i64>,
    #[cfg(target_os = "macos")]
    first: bool,
    #[cfg(target_os = "macos")]
    skip_rasl: bool,
    #[cfg(target_os = "macos")]
    finished: bool,
}

impl Decoder {
    /// `configuration` is the avcC/hvcC payload (not its MP4 box header), as exposed by
    /// `container` (MP4 config `.to_bytes()` or Matroska codec private bytes).
    pub fn new(codec: Codec, configuration: &[u8]) -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            let info = config::StreamInfo::parse(codec, configuration)?;
            let session = ffi::Session::new(&info)?;
            Ok(Self {
                session,
                info,
                pending: Vec::new(),
                in_flight: Vec::new(),
                last_pts: None,
                first: true,
                skip_rasl: false,
                finished: false,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (codec, configuration);
            Err(Error::Unsupported("VideoToolbox requires macOS"))
        }
    }

    /// Submit one complete access unit. PTS uses the caller's consistent tick units; negative
    /// timestamps are supported. Returned frames are monotone by PTS, including across calls.
    /// Errors end the run (do not continue on potentially damaged references).
    pub fn decode(&mut self, sample: &[u8], pts: i64) -> Result<Vec<Frame>> {
        #[cfg(target_os = "macos")]
        {
            if self.finished {
                return Err(Error::Finished);
            }
            let result = self.decode_inner(sample, pts);
            if result.is_err() {
                self.finished = true;
            }
            result
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (sample, pts);
            Err(Error::Unsupported("VideoToolbox requires macOS"))
        }
    }

    /// Finish delayed frames and return the remainder in presentation order. Ends this run;
    /// subsequent calls return `Finished`. Callback and OS errors are not discarded.
    pub fn flush(&mut self) -> Result<Vec<Frame>> {
        #[cfg(target_os = "macos")]
        {
            if self.finished {
                return Err(Error::Finished);
            }
            self.finished = true;
            self.session.finish()?;
            self.collect()?;
            if !self.in_flight.is_empty() {
                return Err(Error::InvalidData("missing decoded picture"));
            }
            self.bump(true)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(Error::Unsupported("VideoToolbox requires macOS"))
        }
    }
}

#[cfg(target_os = "macos")]
impl Decoder {
    fn decode_inner(&mut self, sample: &[u8], pts: i64) -> Result<Vec<Frame>> {
        let kinds = self.info.validate_sample(sample)?;
        if self.first {
            let random_access = match self.info.codec {
                Codec::H264 => kinds.contains(&5),
                Codec::Hevc => kinds.iter().any(|t| (16..=21).contains(t)),
            };
            if !random_access {
                return Err(Error::InvalidData("run must start at a random-access picture"));
            }
            self.skip_rasl = self.info.codec == Codec::Hevc && kinds.iter().any(|t| matches!(t, 16..=18 | 21));
            self.first = false;
        } else if self.info.codec == Codec::Hevc {
            if kinds.iter().any(|t| (16..=21).contains(t)) {
                // BLA always starts a broken-link run, even in the middle of a stream.
                self.skip_rasl = kinds.iter().any(|t| matches!(t, 16..=18));
            }
            if self.skip_rasl && kinds.iter().any(|t| matches!(t, 8 | 9)) {
                return Ok(Vec::new());
            }
        }
        self.session.submit(sample, pts)?;
        self.in_flight.push(pts);
        // Small bounded batch overlaps host work with the hardware engine.
        if self.in_flight.len() >= 8 {
            self.session.wait()?;
        }
        self.collect()?;
        self.bump(false)
    }

    fn collect(&mut self) -> Result<()> {
        for output in self.session.take() {
            let f = output?;
            let i = self
                .in_flight
                .iter()
                .position(|&pts| pts == f.pts)
                .ok_or(Error::InvalidData("unexpected callback timestamp"))?;
            self.in_flight.swap_remove(i);
            let at = self.pending.partition_point(|p| p.pts <= f.pts);
            self.pending.insert(at, f);
        }
        if self.pending.len() + self.in_flight.len() > self.info.reorder + 8 {
            return Err(Error::InvalidData("decoder output queue exceeded limit"));
        }
        Ok(())
    }

    fn bump(&mut self, all: bool) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        while !self.pending.is_empty() && (all || self.pending.len() + self.in_flight.len() > self.info.reorder) {
            let Some(first) = self.pending.first() else { break };
            if self.in_flight.iter().any(|&p| p < first.pts) {
                break;
            }
            if self.last_pts.is_some_and(|p| first.pts < p) {
                return Err(Error::InvalidData("timestamps exceed SPS reorder depth"));
            }
            self.last_pts = Some(first.pts);
            out.push(self.pending.remove(0));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flicks_container::isobmff;

    fn fixture(codec: Codec, bits: u8, full: bool, size: &str, frames: usize) -> (Vec<u8>, Vec<(Vec<u8>, i64)>) {
        let ff = flicks_testkit::ffmpeg().expect("ffmpeg fixture oracle");
        let path = flicks_testkit::temp_path(&flicks_testkit::fixtures_dir("platform").join("video.mp4"));
        let mut cmd = std::process::Command::new(ff);
        cmd.args([
            "-v",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=size={size}:rate=60"),
            "-frames:v",
            &frames.to_string(),
            "-pix_fmt",
            if bits == 10 { "yuv420p10le" } else { "yuv420p" },
            "-color_range",
            if full { "pc" } else { "tv" },
            "-colorspace",
            "bt709",
            "-color_primaries",
            "bt709",
            "-color_trc",
            "bt709",
        ]);
        match codec {
            Codec::H264 => {
                cmd.args([
                    "-c:v",
                    "libx264",
                    "-preset",
                    "fast",
                    "-x264-params",
                    &format!("bframes=3:keyint=24:min-keyint=24:scenecut=0:colorprim=bt709:transfer=bt709:colormatrix=bt709:fullrange={}", if full { "on" } else { "off" }),
                ]);
            }
            Codec::Hevc => {
                cmd.args([
                    "-c:v",
                    "libx265",
                    "-preset",
                    "fast",
                    "-x265-params",
                    &format!("pools=2:frame-threads=2:bframes=3:keyint=24:min-keyint=24:scenecut=0:log-level=error:colorprim=bt709:transfer=bt709:colormatrix=bt709:range={}", if full { "full" } else { "limited" }),
                ]);
            }
        }
        let status = cmd.arg(&path).status().expect("generate fixture");
        assert!(status.success());
        let bytes = std::fs::read(&path).unwrap();
        std::fs::remove_file(path).unwrap();
        let mp4 = isobmff::open(&bytes).unwrap();
        let track = &mp4.tracks[0];
        let config = match track.codec().unwrap() {
            isobmff::CodecConfig::Avc(c) => c.to_bytes(),
            isobmff::CodecConfig::Hevc(c) => c.to_bytes(),
            _ => panic!("video codec"),
        };
        let samples = track
            .samples
            .iter()
            .enumerate()
            .map(|(i, s)| (mp4.read_sample(&bytes, 0, i).unwrap(), s.pts - 1000))
            .collect();
        (config, samples)
    }

    #[cfg(target_os = "macos")]
    fn planar(f: &Frame) -> [Vec<u16>; 3] {
        let planes = f.copy_planes().unwrap();
        let mut out = [Vec::new(), Vec::new(), Vec::new()];
        let (x, y, w, h) = f.info.crop;
        let bps = if f.info.pixel_format == PixelFormat::P010 { 2 } else { 1 };
        let sample = |plane: usize, i: usize| {
            if bps == 1 {
                planes[plane][i] as u16
            } else {
                u16::from_le_bytes([planes[plane][i * 2], planes[plane][i * 2 + 1]]) >> 6
            }
        };
        for row in y..y + h {
            for col in x..x + w {
                out[0].push(sample(0, (row * f.info.width + col) as usize));
            }
        }
        for row in y / 2..y / 2 + h.div_ceil(2) {
            for col in x / 2..x / 2 + w.div_ceil(2) {
                let i = (row * f.info.width.div_ceil(2) * 2 + col * 2) as usize;
                out[1].push(sample(1, i));
                out[2].push(sample(1, i + 1));
            }
        }
        out
    }

    #[cfg(target_os = "macos")]
    fn annexb(info: &config::StreamInfo, sample: &[u8], first: bool) -> Vec<u8> {
        let mut out = Vec::new();
        if first {
            for p in &info.parameter_sets {
                out.extend_from_slice(&[0, 0, 0, 1]);
                out.extend_from_slice(p);
            }
        }
        let mut rest = sample;
        while !rest.is_empty() {
            let n = rest[..info.length_size].iter().fold(0usize, |n, &b| (n << 8) | b as usize);
            rest = &rest[info.length_size..];
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&rest[..n]);
            rest = &rest[n..];
        }
        out
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn generated_planes_match_software() {
        let _ff = flicks_testkit::require_ffmpeg!();
        for (codec, bits) in [(Codec::H264, 8), (Codec::Hevc, 8), (Codec::Hevc, 10)] {
            for full in [false, true] {
                let (config, samples) = fixture(codec, bits, full, "318x242", 32);
                let info = config::StreamInfo::parse(codec, &config).unwrap();
                let mut hw = Decoder::new(codec, &config).unwrap();
                let mut hardware = Vec::new();
                let mut reference = Vec::<(i64, [Vec<u16>; 3])>::new();
                let mut h264 = flicks_h264::Decoder::new();
                let mut hevc = flicks_hevc::Decoder::new();
                for (i, (sample, pts)) in samples.iter().enumerate() {
                    hardware.extend(hw.decode(sample, *pts).unwrap());
                    let data = annexb(&info, sample, i == 0);
                    match codec {
                        Codec::H264 => {
                            for p in h264.decode(&data, *pts).unwrap() {
                                reference
                                    .push((p.pts, [p.y, p.u, p.v].map(|p| p.into_iter().map(u16::from).collect())));
                            }
                        }
                        Codec::Hevc => {
                            for p in hevc.decode(&data, *pts).unwrap() {
                                reference
                                    .push((p.pts, [p.y, p.u, p.v].map(|p| (0..p.len()).map(|i| p.get(i)).collect())));
                            }
                        }
                    }
                }
                hardware.extend(hw.flush().unwrap());
                assert!(matches!(hw.flush(), Err(Error::Finished)));
                match codec {
                    Codec::H264 => {
                        for p in h264.flush() {
                            reference.push((p.pts, [p.y, p.u, p.v].map(|p| p.into_iter().map(u16::from).collect())));
                        }
                    }
                    Codec::Hevc => {
                        for p in hevc.flush() {
                            reference.push((p.pts, [p.y, p.u, p.v].map(|p| (0..p.len()).map(|i| p.get(i)).collect())));
                        }
                    }
                }
                drop(hw); // Retained frames remain readable after their session dies.
                assert_eq!(hardware.len(), 32);
                assert_eq!(hardware.len(), reference.len());
                assert!(hardware.windows(2).all(|w| w[0].pts <= w[1].pts));
                let mut max_error = 0;
                for (h, (pts, s)) in hardware.iter().zip(reference) {
                    assert_eq!(h.pts, pts);
                    assert_eq!((h.info.crop.2, h.info.crop.3), (318, 242));
                    assert_eq!(h.info.color.full_range, full);
                    assert_eq!(h.info.color.primaries, 1);
                    for (a, b) in planar(h).iter().zip(&s) {
                        assert_eq!(a.len(), b.len());
                        for (&a, &b) in a.iter().zip(b) {
                            max_error = max_error.max(a.abs_diff(b));
                        }
                    }
                }
                eprintln!("{codec:?} {bits}-bit full={full}: 32 frames, max plane error {max_error}");
                // Both implementations decode integer samples; there is no colour conversion.
                // Exact comparison is warranted, not a perceptual/PSNR tolerance.
                assert_eq!(max_error, 0);
            }
        }
    }

    #[test]
    fn hostile_configuration_and_samples() {
        let _ff = flicks_testkit::require_ffmpeg!();
        let mut seed = 7u64;
        let mut checked = 0;
        for codec in [Codec::H264, Codec::Hevc] {
            let (record, samples) = fixture(codec, 8, false, "64x64", 2);
            let info = config::StreamInfo::parse(codec, &record).unwrap();
            assert_eq!(info.codec, codec);
            assert_eq!(info.frame.crop.2, 64);
            assert!(info.reorder <= 16);
            for n in 0..record.len() {
                let _ = config::StreamInfo::parse(codec, &record[..n]);
                checked += 1;
            }
            for _ in 0..2000 {
                let mut data = record.clone();
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let i = seed as usize % data.len();
                data[i] ^= (seed >> 32) as u8 | 1;
                let _ = config::StreamInfo::parse(codec, &data);
                checked += 1;
            }
            let sample = &samples[0].0;
            for n in 0..sample.len() {
                let _ = info.validate_sample(&sample[..n]);
                checked += 1;
            }
            for bad in [vec![], vec![0, 0, 0, 0], vec![255; 32], vec![0, 0, 0, 1, 128], vec![0; 16 * 1024 * 1024 + 1]] {
                assert!(info.validate_sample(&bad).is_err());
                #[cfg(target_os = "macos")]
                {
                    let mut decoder = Decoder::new(codec, &record).unwrap();
                    assert!(decoder.decode(&bad, 0).is_err());
                    assert!(matches!(decoder.decode(sample, 0), Err(Error::Finished)));
                }
            }
            #[cfg(target_os = "macos")]
            {
                // Valid framing, damaged compressed picture: exercise OS decoder error handling,
                // including callback failures delivered only by flush and drop with work in flight.
                for cut in [8, 16, 32, 64] {
                    let mut rest = sample.as_slice();
                    let mut corrupt = Vec::new();
                    while !rest.is_empty() {
                        let n = rest[..4].iter().fold(0usize, |n, &b| (n << 8) | b as usize);
                        rest = &rest[4..];
                        let len = n.min(cut);
                        corrupt.extend_from_slice(&(len as u32).to_be_bytes());
                        corrupt.extend_from_slice(&rest[..len]);
                        rest = &rest[n..];
                    }
                    let mut decoder = Decoder::new(codec, &record).unwrap();
                    let result = decoder.decode(&corrupt, 0).and_then(|_| decoder.flush());
                    assert!(result.is_err(), "damaged picture accepted: {codec:?} cut={cut}");
                }
                for _ in 0..16 {
                    let mut decoder = Decoder::new(codec, &record).unwrap();
                    let _ = decoder.decode(sample, 0).unwrap();
                    // Drop while callbacks may still be in flight: wait/invalidate must precede
                    // freeing Shared, independently of whether the caller explicitly flushes.
                }
            }
        }
        eprintln!("hostile configuration/sample cases: {checked}");
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn other_platforms_are_unsupported() {
        assert!(matches!(Decoder::new(Codec::H264, &[]), Err(Error::Unsupported(_))));
        assert!(matches!(Decoder::new(Codec::Hevc, &[]), Err(Error::Unsupported(_))));
    }

    #[test]
    #[ignore = "4K hardware throughput benchmark; run alone in release"]
    #[cfg(target_os = "macos")]
    fn hevc_4k10_throughput() {
        let (config, samples) = fixture(Codec::Hevc, 10, false, "3840x2160", 240);
        for run in 0..4 {
            let start = std::time::Instant::now();
            let mut decoder = Decoder::new(Codec::Hevc, &config).unwrap();
            let mut frames = 0;
            let mut last = i64::MIN;
            for (data, pts) in &samples {
                for f in decoder.decode(data, *pts).unwrap() {
                    assert!(f.pts >= last);
                    last = f.pts;
                    frames += 1;
                }
            }
            for f in decoder.flush().unwrap() {
                assert!(f.pts >= last);
                last = f.pts;
                frames += 1;
            }
            let elapsed = start.elapsed();
            assert_eq!(frames, 240);
            let fps = frames as f64 / elapsed.as_secs_f64();
            eprintln!(
                "4K HEVC Main10 run {run}: {frames} frames / {:.3}s = {fps:.1} fps ({} compressed bytes)",
                elapsed.as_secs_f64(),
                samples.iter().map(|s| s.0.len()).sum::<usize>()
            );
            if run > 0 {
                assert!(fps >= 120.0);
            }
        }
    }
}
