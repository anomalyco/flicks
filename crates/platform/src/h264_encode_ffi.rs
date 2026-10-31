//! One bounded, independently decodable segment. VideoToolbox scales/color-matches
//! directly from the source IOSurface to the compression session's own NV12 pool.
use crate::{Error, Result, ffi::check};
use objc2_core_foundation::{CFArray, CFBoolean, CFData, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms, kCMTimeInvalid,
};
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferPool, kCVImageBufferColorPrimaries_ITU_R_709_2,
    kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::*;
use std::{
    ffi::c_void,
    ptr::{self, NonNull},
    sync::{Mutex, PoisonError},
};

const MAX_FRAMES: usize = 2048;
const MAX_BYTES: usize = 64 << 20;

pub struct VideoSample {
    pub pts_us: i64,
    /// H.264 access unit, four-byte big-endian NAL lengths (not Annex B).
    pub data: Vec<u8>,
    pub idr: bool,
}

#[derive(Default)]
pub struct EncodedVideo {
    /// Exact AVCDecoderConfigurationRecord supplied by the compressor.
    pub avcc: Vec<u8>,
    pub samples: Vec<VideoSample>,
}

#[derive(Default)]
struct Output {
    video: EncodedVideo,
    bytes: usize,
    error: Option<Error>,
}

pub struct H264Encoder {
    session: CFRetained<VTCompressionSession>,
    transfer: CFRetained<VTPixelTransferSession>,
    force: CFRetained<CFDictionary<CFString, CFType>>,
    output: Box<Mutex<Output>>,
    frame_us: u32,
    submitted: usize,
    previous_pts: Option<i64>,
    failed: bool,
}

impl H264Encoder {
    /// H.264 High 4.2, SDR BT.709, no frame reordering. `bitrate` is bits/s,
    /// `frame_us` is the nominal frame duration (for rate control, not a generated clock).
    pub fn new(width: u16, height: u16, bitrate: u32, frame_us: u32) -> Result<Self> {
        if !(2..=1920).contains(&width)
            || !(2..=1080).contains(&height)
            || !width.is_multiple_of(2)
            || !height.is_multiple_of(2)
            || !(64_000..=16_000_000).contains(&bitrate)
            || !(16_666..=1_000_000).contains(&frame_us)
        {
            return Err(Error::Unsupported("H.264 rendition size, bitrate or frame rate"));
        }
        let output = Box::new(Mutex::new(Output::default()));
        let empty = CFDictionary::<CFString, CFType>::empty();
        let pixel_format = CFNumber::new_i32(i32::from_be_bytes(*b"420v"));
        // SAFETY: framework constants have static lifetimes. The typed dictionaries contain
        // the documented values, which Create retains. Output is boxed before registration
        // and Drop drains/invalidates the session before releasing it.
        let (session, transfer, force) = unsafe {
            let spec = CFDictionary::from_slices(
                &[kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder],
                &[CFBoolean::new(true)],
            );
            let attrs = CFDictionary::<CFString, CFType>::from_slices(
                &[kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferIOSurfacePropertiesKey],
                &[pixel_format.as_ref(), empty.as_ref()],
            );
            let mut transfer = ptr::null_mut();
            check(VTPixelTransferSession::create(None, NonNull::from(&mut transfer)), "VTPixelTransferSessionCreate")?;
            let transfer =
                CFRetained::from_raw(NonNull::new(transfer).ok_or(Error::InvalidData("null pixel transfer"))?);
            let force = CFDictionary::<CFString, CFType>::from_slices(
                &[kVTEncodeFrameOptionKey_ForceKeyFrame],
                &[CFBoolean::new(true).as_ref()],
            );
            let mut session = ptr::null_mut();
            check(
                VTCompressionSession::create(
                    None,
                    width.into(),
                    height.into(),
                    u32::from_be_bytes(*b"avc1"),
                    Some(spec.as_ref()),
                    Some(attrs.as_ref()),
                    None,
                    Some(output_callback),
                    (&*output as *const Mutex<Output>).cast_mut().cast(),
                    NonNull::from(&mut session),
                ),
                "VTCompressionSessionCreate",
            )?;
            let session =
                CFRetained::from_raw(NonNull::new(session).ok_or(Error::InvalidData("null compression session"))?);
            (session, transfer, force)
        };
        let encoder =
            Self { session, transfer, force, output, frame_us, submitted: 0, previous_pts: None, failed: false };
        let avg = CFNumber::new_i32(bitrate as i32);
        let fps = CFNumber::new_f64(1_000_000.0 / f64::from(frame_us));
        // 125% of target over a second. HLS BANDWIDTH also allows for the AAC rendition and boxes.
        let max_bytes = CFNumber::new_i32((bitrate * 5 / 4 / 8) as i32);
        let second = CFNumber::new_i32(1);
        let limit = CFArray::from_objects(&[&*max_bytes, &*second]);
        // SAFETY: both are live VT sessions; keys and CF values have the types documented
        // for those sessions. Setting the destination colorimetry requests pixel conversion,
        // not just output tags. The encoder and sample entry describe that same result.
        unsafe {
            let compression = CFDictionary::<CFString, CFType>::from_slices(
                &[
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    kVTCompressionPropertyKey_AverageBitRate,
                    kVTCompressionPropertyKey_DataRateLimits,
                    kVTCompressionPropertyKey_ExpectedFrameRate,
                    kVTCompressionPropertyKey_ColorPrimaries,
                    kVTCompressionPropertyKey_TransferFunction,
                    kVTCompressionPropertyKey_YCbCrMatrix,
                ],
                &[
                    kVTProfileLevel_H264_High_4_2.as_ref(),
                    CFBoolean::new(false).as_ref(),
                    avg.as_ref(),
                    limit.as_ref(),
                    fps.as_ref(),
                    kCVImageBufferColorPrimaries_ITU_R_709_2.as_ref(),
                    kCVImageBufferTransferFunction_ITU_R_709_2.as_ref(),
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2.as_ref(),
                ],
            );
            check(
                VTSessionSetProperties(encoder.session.as_ref(), compression.as_ref()),
                "H.264 compression properties",
            )?;
            let transfer = CFDictionary::<CFString, CFString>::from_slices(
                &[
                    kVTPixelTransferPropertyKey_ScalingMode,
                    kVTPixelTransferPropertyKey_DownsamplingMode,
                    kVTPixelTransferPropertyKey_DestinationColorPrimaries,
                    kVTPixelTransferPropertyKey_DestinationTransferFunction,
                    kVTPixelTransferPropertyKey_DestinationYCbCrMatrix,
                ],
                &[
                    kVTScalingMode_CropSourceToCleanAperture,
                    kVTDownsamplingMode_Average,
                    kCVImageBufferColorPrimaries_ITU_R_709_2,
                    kCVImageBufferTransferFunction_ITU_R_709_2,
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2,
                ],
            );
            check(
                VTSessionSetProperties(encoder.transfer.as_ref(), transfer.as_ref()),
                "SDR pixel transfer properties",
            )?;
            check(encoder.session.prepare_to_encode_frames(), "VTCompressionSessionPrepareToEncodeFrames")?;
        }
        Ok(encoder)
    }

    /// Submit a tagged source image in presentation order (timestamps in microseconds).
    /// The first image always requests an IDR. Errors terminate this encoding run.
    pub fn encode(&mut self, image: &CVPixelBuffer, pts_us: i64, force_idr: bool) -> Result<()> {
        if self.failed {
            return Err(Error::Finished);
        }
        let result = self.submit(image, pts_us, force_idr);
        self.failed = result.is_err();
        result
    }

    fn submit(&mut self, image: &CVPixelBuffer, pts: i64, force: bool) -> Result<()> {
        if self.submitted >= MAX_FRAMES || self.previous_pts.is_some_and(|p| p >= pts) {
            return Err(Error::InvalidData("encoder frame count or timestamp"));
        }
        let timestamp = |value| CMTime { value, timescale: 1_000_000, flags: CMTimeFlags::Valid, epoch: 0 };
        // SAFETY: the pool belongs to the retained session. Its Create call writes a +1
        // destination buffer, distinct from the borrowed source. Transfer is synchronous;
        // encode_frame retains the pool buffer until the hardware is finished reading it.
        unsafe {
            let pool = self.session.pixel_buffer_pool().ok_or(Error::Unsupported("no NV12 encoder pool"))?;
            let mut scaled = ptr::null_mut();
            check(
                CVPixelBufferPool::create_pixel_buffer(None, &pool, NonNull::from(&mut scaled)),
                "encoder pool allocation",
            )?;
            let scaled = CFRetained::from_raw(NonNull::new(scaled).ok_or(Error::InvalidData("null encoder buffer"))?);
            check(self.transfer.transfer_image(image, &scaled), "VTPixelTransferSessionTransferImage")?;
            let properties = (force || self.submitted == 0).then(|| self.force.as_ref());
            check(
                self.session.encode_frame(
                    &scaled,
                    timestamp(pts),
                    timestamp(i64::from(self.frame_us)),
                    properties,
                    ptr::null_mut(),
                    ptr::null_mut(),
                ),
                "VTCompressionSessionEncodeFrame",
            )?;
        }
        self.submitted += 1;
        self.previous_pts = Some(pts);
        // Don't queue the whole segment's pixel buffers if the encoder falls behind.
        if self.submitted.is_multiple_of(8) {
            self.complete()?;
        }
        Ok(())
    }

    fn complete(&mut self) -> Result<()> {
        // SAFETY: kCMTimeInvalid asks for all outstanding callbacks to finish. No concurrent submit.
        check(unsafe { self.session.complete_frames(kCMTimeInvalid) }, "VTCompressionSessionCompleteFrames")?;
        let mut output = self.output.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(error) = output.error.take() {
            return Err(error);
        }
        if output.video.samples.len() != self.submitted {
            return Err(Error::InvalidData("missing compressed frame"));
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<EncodedVideo> {
        if self.failed || self.submitted == 0 {
            return Err(Error::Finished);
        }
        self.complete()?;
        Ok(std::mem::take(&mut self.output.lock().unwrap_or_else(PoisonError::into_inner).video))
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // SAFETY: sessions are retained; invalidate before freeing the boxed callback context,
        // including on early errors when frames might still be in flight.
        unsafe {
            self.session.complete_frames(kCMTimeInvalid);
            self.session.invalidate();
            self.transfer.invalidate();
        }
    }
}

unsafe extern "C-unwind" fn output_callback(
    context: *mut c_void,
    _: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    // SAFETY: only H264Encoder::new registers this callback, with a stable Box; the owning
    // encoder waits for callbacks before invalidating the session and dropping the Box.
    let Some(shared) = (unsafe { context.cast::<Mutex<Output>>().as_ref() }) else { return };
    let mut output = shared.lock().unwrap_or_else(PoisonError::into_inner);
    if output.error.is_some() {
        return;
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check(status, "H.264 output callback")?;
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Err(Error::InvalidData("encoder dropped frame"));
        }
        // SAFETY: non-null CMSampleBuffer delivered by VT is valid throughout the callback.
        let sample = unsafe { sample.as_ref() }.ok_or(Error::InvalidData("missing compressed sample"))?;
        output.append(sample)
    }))
    .unwrap_or(Err(Error::InvalidData("invalid encoder callback")));
    output.error = result.err();
}

impl Output {
    fn append(&mut self, sample: &CMSampleBuffer) -> Result<()> {
        if self.video.samples.len() >= MAX_FRAMES {
            return Err(Error::InvalidData("encoder output count"));
        }
        // SAFETY: this is a live immutable sample buffer from our H.264 compression session;
        // all returned CoreMedia objects are retained for extraction and copying.
        let (block, pts) = unsafe {
            (
                sample.data_buffer().ok_or(Error::InvalidData("missing compressed data"))?,
                sample.presentation_time_stamp(),
            )
        };
        if !pts.flags.contains(CMTimeFlags::Valid) || pts.timescale <= 0 {
            return Err(Error::InvalidData("encoder PTS"));
        }
        let pts_us = i64::try_from(i128::from(pts.value) * 1_000_000 / i128::from(pts.timescale))
            .map_err(|_| Error::InvalidData("encoder timestamp overflow"))?;
        // SAFETY: the block is retained and the destination has exactly len writable bytes.
        let len = unsafe { block.data_length() };
        if len == 0 || len > 16 << 20 || len > MAX_BYTES - self.bytes {
            return Err(Error::InvalidData("encoder output size"));
        }
        let mut data = vec![0u8; len];
        check(
            // SAFETY: positive length checked above; Copy supports non-contiguous CMBlockBuffers.
            unsafe { block.copy_data_bytes(0, len, NonNull::from(data.as_mut_slice()).cast()) },
            "CMBlockBufferCopyDataBytes",
        )?;
        if self.video.avcc.is_empty() {
            self.video.avcc = configuration(sample)?;
        }
        let mut rest = data.as_slice();
        let mut idr = false;
        while !rest.is_empty() {
            let (prefix, payload) = rest.split_at_checked(4).ok_or(Error::InvalidData("AVC sample length"))?;
            let size = u32::from_be_bytes(prefix.try_into().map_err(|_| Error::InvalidData("AVC length"))?) as usize;
            let (nal, next) = payload.split_at_checked(size).ok_or(Error::InvalidData("AVC NAL length"))?;
            idr |= nal.first().ok_or(Error::InvalidData("empty AVC NAL"))? & 31 == 5;
            rest = next;
        }
        if self.video.samples.is_empty() && !idr {
            return Err(Error::InvalidData("segment does not start with IDR"));
        }
        self.bytes += len;
        self.video.samples.push(VideoSample { pts_us, data, idr });
        Ok(())
    }
}

fn configuration(sample: &CMSampleBuffer) -> Result<Vec<u8>> {
    let bad = || Error::InvalidData("missing or invalid encoder avcC");
    // SAFETY: this H.264 sample's retained format description contains an immutable CF
    // property-list dictionary of extension atom payloads. Downcast both levels before
    // reading bytes; the dictionary retains the value through the copy.
    unsafe {
        let format = sample.format_description().ok_or_else(bad)?;
        let atoms = format.extension(kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms).ok_or_else(bad)?;
        let dict = atoms.downcast_ref::<CFDictionary>().ok_or_else(bad)?;
        let key = CFString::from_str("avcC");
        let value = dict.value((&*key as *const CFString).cast()).cast::<CFType>().as_ref().ok_or_else(bad)?;
        let data = value.downcast_ref::<CFData>().filter(|d| (7..=65_535).contains(&d.len())).ok_or_else(bad)?.to_vec();
        if data.first() != Some(&1) || data.get(4).is_none_or(|b| b & 3 != 3) {
            return Err(bad());
        }
        Ok(data)
    }
}
