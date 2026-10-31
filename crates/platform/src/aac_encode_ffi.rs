//! Bounded, one-shot AAC-LC stereo encoding with Apple's AudioConverter.

use std::{ffi::c_void, ptr::NonNull};

use objc2_audio_toolbox::*;
use objc2_core_audio_types::*;

use crate::{Error, Result};

/// One independent AAC-LC encoding run. The caller must account for
/// `priming_frames()` in the returned raw access units (1024 samples each).
pub struct AacEncoder {
    converter: NonNull<OpaqueAudioConverter>,
    rate: u32,
    priming: u32,
    packet_bytes: u32,
}

impl AacEncoder {
    pub fn new(rate: u32) -> Result<Self> {
        if ![8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000].contains(&rate) {
            return Err(Error::Unsupported("AAC sample rate"));
        }
        let mut input = AudioStreamBasicDescription {
            mSampleRate: f64::from(rate),
            mFormatID: kAudioFormatLinearPCM,
            mFormatFlags: kAudioFormatFlagIsFloat | kAudioFormatFlagIsPacked,
            mBytesPerPacket: 8,
            mFramesPerPacket: 1,
            mBytesPerFrame: 8,
            mChannelsPerFrame: 2,
            mBitsPerChannel: 32,
            mReserved: 0,
        };
        let mut output = AudioStreamBasicDescription {
            mSampleRate: f64::from(rate),
            mFormatID: kAudioFormatMPEG4AAC,
            mFramesPerPacket: 1024,
            mChannelsPerFrame: 2,
            mFormatFlags: 0,
            mBytesPerPacket: 0,
            mBytesPerFrame: 0,
            mBitsPerChannel: 0,
            mReserved: 0,
        };
        let mut converter = std::ptr::null_mut();
        // SAFETY: all descriptions and the out pointer are initialized and live for this call.
        status("AudioConverterNew", unsafe {
            AudioConverterNew(NonNull::from(&mut input), NonNull::from(&mut output), NonNull::from(&mut converter))
        })?;
        let converter = NonNull::new(converter).ok_or(Error::InvalidData("null audio converter"))?;
        let mut encoder = Self { converter, rate, priming: 0, packet_bytes: 0 };
        let mut bitrate = (rate * 4).min(192_000);
        // SAFETY: the converter is owned, and bitrate points to a u32 of the specified size.
        status("AAC bitrate", unsafe {
            AudioConverterSetProperty(
                converter.as_ptr(),
                kAudioConverterEncodeBitRate,
                4,
                NonNull::from(&mut bitrate).cast(),
            )
        })?;
        let mut prime = AudioConverterPrimeInfo { leadingFrames: 0, trailingFrames: 0 };
        let mut size = std::mem::size_of_val(&prime) as u32;
        // SAFETY: prime and size are valid writable storage of the declared size.
        status("AAC priming", unsafe {
            AudioConverterGetProperty(
                converter.as_ptr(),
                kAudioConverterPrimeInfo,
                NonNull::from(&mut size),
                NonNull::from(&mut prime).cast(),
            )
        })?;
        if size as usize != std::mem::size_of_val(&prime) || prime.leadingFrames > 8192 || prime.trailingFrames > 8192 {
            return Err(Error::Unsupported("AAC priming size"));
        }
        encoder.priming = prime.leadingFrames;
        size = 4;
        // SAFETY: packet_bytes is writable u32 storage; the converter is still owned.
        status("AAC packet size", unsafe {
            AudioConverterGetProperty(
                converter.as_ptr(),
                kAudioConverterPropertyMaximumOutputPacketSize,
                NonNull::from(&mut size),
                NonNull::from(&mut encoder.packet_bytes).cast(),
            )
        })?;
        if size != 4 || encoder.packet_bytes == 0 || encoder.packet_bytes > 65_536 {
            return Err(Error::Unsupported("AAC packet size"));
        }
        Ok(encoder)
    }

    pub fn priming_frames(&self) -> u32 {
        self.priming
    }

    /// Interleaved stereo f32, at most 33 seconds including preroll. Consumes the
    /// converter, flushes delayed packets, and never retains a pointer to `pcm`.
    pub fn encode(self, pcm: &[f32]) -> Result<Vec<Vec<u8>>> {
        if pcm.is_empty()
            || !pcm.len().is_multiple_of(2)
            || pcm.len() > self.rate as usize * 33 * 2
            || pcm.iter().any(|x| !x.is_finite())
        {
            return Err(Error::InvalidData("AAC PCM size or samples"));
        }
        let mut input = Input { pcm, offset: 0 };
        let mut packets = Vec::new();
        let mut buffer = vec![0u8; self.packet_bytes as usize];
        let limit = (pcm.len() / 2).div_ceil(1024) + 32;
        let mut bytes = 0usize;
        for _ in 0..limit {
            let mut count = 1;
            let mut description =
                AudioStreamPacketDescription { mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: 0 };
            let mut output = AudioBufferList {
                mNumberBuffers: 1,
                mBuffers: [AudioBuffer {
                    mNumberChannels: 2,
                    mDataByteSize: self.packet_bytes,
                    mData: buffer.as_mut_ptr().cast(),
                }],
            };
            // SAFETY: the synchronous callback borrows input/pcm only during this call;
            // output has space for one maximum-sized packet and one description.
            status("AAC encode", unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter.as_ptr(),
                    Some(provide_input),
                    (&mut input as *mut Input<'_>).cast(),
                    NonNull::from(&mut count),
                    NonNull::from(&mut output),
                    &mut description,
                )
            })?;
            if count == 0 {
                return if input.offset == pcm.len() {
                    Ok(packets)
                } else {
                    Err(Error::InvalidData("AAC ended before consuming PCM"))
                };
            }
            if count != 1 || description.mStartOffset < 0 || !matches!(description.mVariableFramesInPacket, 0 | 1024) {
                return Err(Error::InvalidData("AAC output packet description"));
            }
            let start = description.mStartOffset as usize;
            let end =
                start.checked_add(description.mDataByteSize as usize).ok_or(Error::InvalidData("AAC output size"))?;
            let data = buffer.get(start..end).filter(|p| !p.is_empty()).ok_or(Error::InvalidData("AAC output size"))?;
            bytes += data.len();
            if bytes > 64 * 1024 * 1024 {
                return Err(Error::InvalidData("AAC output limit"));
            }
            packets.push(data.to_vec());
        }
        Err(Error::InvalidData("AAC encoder did not finish"))
    }
}

impl Drop for AacEncoder {
    fn drop(&mut self) {
        // SAFETY: this is the sole owner; FillComplexBuffer and its callbacks have returned.
        unsafe {
            AudioConverterDispose(self.converter.as_ptr());
        }
    }
}

struct Input<'a> {
    pcm: &'a [f32],
    offset: usize,
}

unsafe extern "C-unwind" fn provide_input(
    _: AudioConverterRef,
    mut packets: NonNull<u32>,
    mut data: NonNull<AudioBufferList>,
    descriptions: *mut *mut AudioStreamPacketDescription,
    context: *mut c_void,
) -> i32 {
    // SAFETY: only encode calls this callback, synchronously, with live Input and
    // valid AudioConverter callback storage. The PCM borrow lasts through the call.
    unsafe {
        let input = &mut *context.cast::<Input<'_>>();
        let remaining = input.pcm.get(input.offset..).unwrap_or(&[]);
        let frames = (*packets.as_ref() as usize).min(remaining.len() / 2);
        *packets.as_mut() = frames as u32;
        *data.as_mut() = AudioBufferList {
            mNumberBuffers: 1,
            mBuffers: [AudioBuffer {
                mNumberChannels: 2,
                mDataByteSize: (frames * 8) as u32,
                mData: if frames == 0 { std::ptr::null_mut() } else { remaining.as_ptr().cast_mut().cast() },
            }],
        };
        input.offset += frames * 2;
        if !descriptions.is_null() {
            *descriptions = std::ptr::null_mut();
        }
    }
    0
}

fn status(operation: &'static str, status: i32) -> Result<()> {
    if status == 0 { Ok(()) } else { Err(Error::Platform { operation, status }) }
}
