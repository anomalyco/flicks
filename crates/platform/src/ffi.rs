//! VideoToolbox FFI adapted from FilmCraft fed0f42 (MIT OR Apache-2.0).
//! Unlike the source, retain the IOSurface-backed buffer instead of copying pixels in callbacks.
use crate::{Codec, Error, Frame, FrameInfo, PixelFormat, Result, config::StreamInfo};
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo, CMTime, CMTimeFlags,
    CMVideoFormatDescriptionCreateFromH264ParameterSets, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
    kCMBlockBufferAssureMemoryNowFlag,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBuffer, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane,
    CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetIOSurface, CVPixelBufferGetPixelFormatType,
    CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVPixelBufferIOSurfacePropertiesKey,
    kCVPixelBufferPixelFormatTypeKey,
};
use objc2_video_toolbox::{
    VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord, VTDecompressionSession,
    kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
};
use std::{
    ffi::c_void,
    ptr::NonNull,
    sync::{Mutex, PoisonError},
};

fn check(status: i32, operation: &'static str) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(Error::Platform { operation, status }) }
}

/// Request macOS UI scheduling for the calling thread; leave decode and I/O on their own workers.
pub fn set_user_interactive_qos() -> Result<()> {
    const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21; // <sys/qos.h>
    // SAFETY: macOS <pthread/qos.h> takes qos_class_t (unsigned int) and int, and returns int.
    unsafe extern "C" {
        fn pthread_set_qos_class_self_np(class: u32, relative_priority: i32) -> i32;
    }
    check(
        // SAFETY: this takes two scalars, a valid QoS class and a relative priority in [-15, 0].
        unsafe { pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0) },
        "pthread_set_qos_class_self_np",
    )
}

fn pixel_format(info: &FrameInfo) -> u32 {
    u32::from_be_bytes(match (info.pixel_format, info.color.full_range) {
        (PixelFormat::Nv12, false) => *b"420v",
        (PixelFormat::Nv12, true) => *b"420f",
        (PixelFormat::P010, false) => *b"x420",
        (PixelFormat::P010, true) => *b"xf20",
    })
}

/// Only created by a successful output callback. No mutable pixel access is exposed here.
pub(crate) struct Buffer(CFRetained<CVPixelBuffer>);

// SAFETY: the retained pixel buffer has completed decompression and is immutable in this API.
// Ownership may move between threads; we deliberately do not implement Sync or Clone.
unsafe impl Send for Buffer {}

impl Buffer {
    pub fn pixel_buffer(&self) -> &CVPixelBuffer {
        &self.0
    }

    pub fn copy_planes(&self) -> Result<[Vec<u8>; 2]> {
        check(
            // SAFETY: the retained buffer remains alive throughout the matching lock/unlock scope.
            unsafe { CVPixelBufferLockBaseAddress(&self.0, CVPixelBufferLockFlags::ReadOnly) },
            "CVPixelBufferLockBaseAddress",
        )?;
        let _lock = Locked(&self.0);
        let mut planes = [Vec::new(), Vec::new()];
        let bps = match CVPixelBufferGetPixelFormatType(&self.0) {
            x if x == u32::from_be_bytes(*b"420v") || x == u32::from_be_bytes(*b"420f") => 1,
            x if x == u32::from_be_bytes(*b"x420") || x == u32::from_be_bytes(*b"xf20") => 2,
            _ => return Err(Error::InvalidData("unexpected pixel format")),
        };
        for (i, output) in planes.iter_mut().enumerate() {
            let width = CVPixelBufferGetWidthOfPlane(&self.0, i);
            let height = CVPixelBufferGetHeightOfPlane(&self.0, i);
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&self.0, i);
            let row = width.checked_mul(bps * (i + 1)).ok_or(Error::InvalidData("plane size overflow"))?;
            let len = stride
                .checked_mul(height)
                .filter(|&n| n <= 256 * 1024 * 1024)
                .ok_or(Error::InvalidData("plane size overflow"))?;
            let base = CVPixelBufferGetBaseAddressOfPlane(&self.0, i) as *const u8;
            if base.is_null() || row > stride {
                return Err(Error::InvalidData("unmapped plane"));
            }
            for y in 0..height {
                let offset = y * stride;
                if offset.checked_add(row).is_none_or(|end| end > len) {
                    return Err(Error::InvalidData("short plane"));
                }
                // SAFETY: CoreVideo provides width samples at each stride-spaced row while
                // locked. Read only visible bytes, not padding beyond the final row of a view.
                // The checked offset and length are bounded below isize::MAX.
                let data = unsafe { std::slice::from_raw_parts(base.add(offset), row) };
                output.extend_from_slice(data);
            }
        }
        Ok(planes)
    }
}

struct Locked<'a>(&'a CVPixelBuffer);
impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: paired with a successful read-only lock; the borrow keeps the buffer alive.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags::ReadOnly) };
    }
}

struct Shared {
    info: FrameInfo,
    out: Mutex<Vec<Result<Frame>>>,
}

pub(crate) struct Session {
    session: CFRetained<VTDecompressionSession>,
    format: CFRetained<CMFormatDescription>,
    /// Stable callback address, alive until wait + invalidation in Drop.
    shared: Box<Shared>,
}

// SAFETY: VideoToolbox sessions/CoreMedia sample objects support use on different threads;
// the owner uses &mut Decoder serially, and only Shared.out is accessed from callbacks.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: valid retained session. Wait completes callbacks, then invalidation prevents
        // new callbacks before the Box<Shared> is freed. No fallible cleanup can be returned here.
        unsafe {
            self.session.wait_for_asynchronous_frames();
            self.session.invalidate();
        }
    }
}

// SAFETY: registered only with a stable Shared pointer owned by Session; VideoToolbox supplies
// a valid optional image for the duration of the callback. Never unwind across this boundary.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _frame_refcon: *mut c_void,
    status: i32,
    _flags: VTDecodeInfoFlags,
    image: *mut CVImageBuffer,
    pts: CMTime,
    _duration: CMTime,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: Session owns the stable Box and waits/invalidates before releasing it.
    let shared = unsafe { &*(refcon as *const Shared) };
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check(status, "VideoToolbox output callback")?;
        let image = NonNull::new(image).ok_or(Error::InvalidData("VideoToolbox dropped a picture"))?;
        // SAFETY: callback image is valid until callback returns; retain it for the returned frame.
        let pb = unsafe { CFRetained::retain(image) };
        if CVPixelBufferGetPlaneCount(&pb) != 2 || CVPixelBufferGetPixelFormatType(&pb) != pixel_format(&shared.info) {
            return Err(Error::InvalidData("unexpected decoded pixel format"));
        }
        if CVPixelBufferGetIOSurface(Some(&pb)).is_none() {
            return Err(Error::InvalidData("decoded buffer has no IOSurface"));
        }
        let mut info = shared.info;
        let (w, h) = (CVPixelBufferGetWidth(&pb), CVPixelBufferGetHeight(&pb));
        if (w, h) == (info.crop.2 as usize, info.crop.3 as usize) {
            info.crop.0 = 0;
            info.crop.1 = 0;
        } else if (w, h) != (info.width as usize, info.height as usize) {
            return Err(Error::InvalidData("unexpected decoded dimensions"));
        }
        info.width = w as u32;
        info.height = h as u32;
        if pts.timescale != 1 || !pts.flags.contains(CMTimeFlags::Valid) {
            return Err(Error::InvalidData("invalid output timestamp"));
        }
        Ok(Frame { pts: pts.value, info, buffer: Buffer(pb) })
    }))
    .unwrap_or(Err(Error::InvalidData("panic in output callback")));
    // No user code runs under this lock; poison recovery is safe. Submissions are bounded at 8.
    shared.out.lock().unwrap_or_else(PoisonError::into_inner).push(result);
}

impl Session {
    pub fn new(info: &StreamInfo) -> Result<Self> {
        let format = format_description(info)?;
        // SAFETY: immutable Apple framework constants, initialized before use.
        let (hw_key, pf_key, io_key) = unsafe {
            (
                kVTVideoDecoderSpecification_RequireHardwareAcceleratedVideoDecoder,
                kCVPixelBufferPixelFormatTypeKey,
                kCVPixelBufferIOSurfacePropertiesKey,
            )
        };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[hw_key], &[CFBoolean::new(true).as_ref()]);
        let pf = CFNumber::new_i32(pixel_format(&info.frame) as i32);
        let io = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        let attrs = CFDictionary::<CFString, CFType>::from_slices(&[pf_key, io_key], &[pf.as_ref(), io.as_ref()]);
        let shared = Box::new(Shared { info: info.frame, out: Mutex::new(Vec::new()) });
        let record = VTDecompressionOutputCallbackRecord {
            decompressionOutputCallback: Some(output_callback),
            decompressionOutputRefCon: &*shared as *const Shared as *mut c_void,
        };
        let mut out = std::ptr::null_mut();
        check(
            // SAFETY: all pointers live for this call; VT copies the record and dictionaries.
            // The callback refcon remains owned by Session until its Drop waits and invalidates.
            unsafe {
                VTDecompressionSession::create(
                    None,
                    &format,
                    Some(spec.as_ref()),
                    Some(attrs.as_ref()),
                    &record,
                    NonNull::from(&mut out),
                )
            },
            "VTDecompressionSessionCreate",
        )?;
        let out = NonNull::new(out).ok_or(Error::InvalidData("null decompression session"))?;
        // SAFETY: Create returned a +1 retained session, ownership transferred to CFRetained.
        let session = unsafe { CFRetained::from_raw(out) };
        Ok(Self { session, format, shared })
    }

    pub fn submit(&self, sample: &[u8], pts: i64) -> Result<()> {
        let buffer = sample_buffer(sample, pts, &self.format)?;
        let mut flags = VTDecodeInfoFlags(0);
        check(
            // SAFETY: valid session and copied sample; VT retains input until asynchronous completion.
            unsafe {
                self.session.decode_frame(
                    &buffer,
                    VTDecodeFrameFlags::Frame_EnableAsynchronousDecompression,
                    std::ptr::null_mut(),
                    &mut flags,
                )
            },
            "VTDecompressionSessionDecodeFrame",
        )?;
        if flags.contains(VTDecodeInfoFlags::FrameDropped) {
            return Err(Error::InvalidData("VideoToolbox dropped input"));
        }
        Ok(())
    }

    pub fn wait(&self) -> Result<()> {
        // SAFETY: retained session is live; callbacks can safely use the boxed Shared.
        check(unsafe { self.session.wait_for_asynchronous_frames() }, "VTDecompressionSessionWaitForAsynchronousFrames")
    }

    pub fn finish(&self) -> Result<()> {
        // SAFETY: valid session, no concurrent submits (exclusive Decoder borrow).
        check(unsafe { self.session.finish_delayed_frames() }, "VTDecompressionSessionFinishDelayedFrames")?;
        self.wait()
    }

    pub fn take(&self) -> Vec<Result<Frame>> {
        std::mem::take(&mut *self.shared.out.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

fn format_description(info: &StreamInfo) -> Result<CFRetained<CMFormatDescription>> {
    let mut ptrs = Vec::with_capacity(info.parameter_sets.len());
    let mut sizes = Vec::with_capacity(info.parameter_sets.len());
    for p in &info.parameter_sets {
        ptrs.push(NonNull::new(p.as_ptr() as *mut u8).ok_or(Error::InvalidData("empty parameter set"))?);
        sizes.push(p.len());
    }
    let ptr_array = NonNull::new(ptrs.as_mut_ptr()).ok_or(Error::InvalidData("no parameter sets"))?;
    let size_array = NonNull::new(sizes.as_mut_ptr()).ok_or(Error::InvalidData("no parameter sets"))?;
    let mut out = std::ptr::null();
    // SAFETY: ptrs/sizes contain matching, validated NAL slices alive for this call. CoreMedia
    // copies the parameter sets and writes a retained description to out on success.
    let status = unsafe {
        match info.codec {
            Codec::H264 => CMVideoFormatDescriptionCreateFromH264ParameterSets(
                None,
                ptrs.len(),
                ptr_array,
                size_array,
                info.length_size as i32,
                NonNull::from(&mut out),
            ),
            Codec::Hevc => CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                ptrs.len(),
                ptr_array,
                size_array,
                info.length_size as i32,
                None,
                NonNull::from(&mut out),
            ),
        }
    };
    check(status, "CMVideoFormatDescriptionCreateFromParameterSets")?;
    let out = NonNull::new(out as *mut CMFormatDescription).ok_or(Error::InvalidData("null format description"))?;
    // SAFETY: Create returned +1; CFRetained takes ownership.
    Ok(unsafe { CFRetained::from_raw(out) })
}

fn sample_buffer(sample: &[u8], pts: i64, format: &CMFormatDescription) -> Result<CFRetained<CMSampleBuffer>> {
    let mut block = std::ptr::null_mut();
    check(
        // SAFETY: CoreMedia allocates exactly sample.len() bytes with no external backing memory.
        unsafe {
            CMBlockBuffer::create_with_memory_block(
                None,
                std::ptr::null_mut(),
                sample.len(),
                None,
                std::ptr::null(),
                0,
                sample.len(),
                kCMBlockBufferAssureMemoryNowFlag,
                NonNull::from(&mut block),
            )
        },
        "CMBlockBufferCreateWithMemoryBlock",
    )?;
    let block = NonNull::new(block).ok_or(Error::InvalidData("null block buffer"))?;
    // SAFETY: Create returned +1 retained block.
    let block = unsafe { CFRetained::from_raw(block) };
    let src = NonNull::new(sample.as_ptr() as *mut c_void).ok_or(Error::InvalidData("empty sample"))?;
    // SAFETY: copies the validated sample into the block of the same length before sample expires.
    check(unsafe { CMBlockBuffer::replace_data_bytes(src, &block, 0, sample.len()) }, "CMBlockBufferReplaceDataBytes")?;
    let invalid = CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 };
    let timing = CMSampleTimingInfo {
        duration: invalid,
        decodeTimeStamp: invalid,
        presentationTimeStamp: CMTime { value: pts, timescale: 1, flags: CMTimeFlags::Valid, epoch: 0 },
    };
    let size = sample.len();
    let mut out = std::ptr::null_mut();
    check(
        // SAFETY: one sample/timing/size, pointing to live locals; the sample buffer retains block
        // and format. CoreMedia writes a +1 retained sample buffer on success.
        unsafe {
            CMSampleBuffer::create_ready(
                None,
                Some(&block),
                Some(format),
                1,
                1,
                &timing,
                1,
                &size,
                NonNull::from(&mut out),
            )
        },
        "CMSampleBufferCreateReady",
    )?;
    let out = NonNull::new(out).ok_or(Error::InvalidData("null sample buffer"))?;
    // SAFETY: Create returned +1; CFRetained takes ownership.
    Ok(unsafe { CFRetained::from_raw(out) })
}
