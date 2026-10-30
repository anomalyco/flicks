//! The only FFI/unsafe boundary: CoreVideo planes → same-device Metal textures → wgpu.
use super::*;
use objc2_core_foundation::CFRetained;
use objc2_core_video::*;
use objc2_metal::{MTLPixelFormat, MTLTextureType};
use std::ptr::{NonNull, null_mut};

pub(super) struct Importer {
    device: wgpu::Device,
    cache: CFRetained<CVMetalTextureCache>,
}

// wgpu's HAL texture owns this guard, including while a submitted command uses the texture.
// Keeping just MTLTexture is insufficient: CoreVideo owns the IOSurface mapping and buffer pool.
struct PlaneOwner {
    _texture: CFRetained<CVMetalTexture>,
    _buffer: CFRetained<CVPixelBuffer>,
}
// SAFETY: These retained CF objects are only released, never read or mutated, by the drop
// callback. CF retain/release is thread-safe. No CVPixelBuffer pointer escapes this guard.
unsafe impl Send for PlaneOwner {}
// SAFETY: The guard exposes no access; its CF owners remain immutable until its one drop.
unsafe impl Sync for PlaneOwner {}

impl Importer {
    pub(super) fn new(device: &wgpu::Device) -> Result<Self, Error> {
        // SAFETY: Only borrow the device to create a cache; never release/modify its raw handle.
        let metal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }
            .ok_or_else(|| Error::Import("requires a Metal device".into()))?;
        let mut raw = null_mut();
        // SAFETY: The live raw device belongs to wgpu, output pointer is valid, optional
        // dictionaries and allocator are null. CoreVideo retains the device in the new cache.
        check(unsafe { CVMetalTextureCache::create(None, None, metal.raw_device(), None, NonNull::from(&mut raw)) })?;
        let raw = NonNull::new(raw).ok_or_else(|| Error::Import("null cache".into()))?;
        // SAFETY: Successful Create returns one owned CF reference; adopt it exactly once.
        let cache = unsafe { CFRetained::from_raw(raw) };
        Ok(Self { device: device.clone(), cache })
    }

    #[allow(non_upper_case_globals)]
    pub(super) fn import(&self, buffer: &CVPixelBuffer, mut color: Color) -> Result<Frame, Error> {
        let (format, range) = match CVPixelBufferGetPixelFormatType(buffer) {
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange => (PixelFormat::Nv12, Range::Limited),
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange => (PixelFormat::Nv12, Range::Full),
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange => (PixelFormat::P010, Range::Limited),
            kCVPixelFormatType_420YpCbCr10BiPlanarFullRange => (PixelFormat::P010, Range::Full),
            other => return Err(Error::Import(format!("unsupported fourcc {other:#x}"))),
        };
        color.range = range;
        let width = u32::try_from(CVPixelBufferGetWidth(buffer)).map_err(|_| Error::Frame("width overflow"))?;
        let height = u32::try_from(CVPixelBufferGetHeight(buffer)).map_err(|_| Error::Frame("height overflow"))?;
        validate_size(&self.device, [width, height], format)?;
        if CVPixelBufferGetPlaneCount(buffer) != 2 || CVPixelBufferGetIOSurface(Some(buffer)).is_none() {
            return Err(Error::Import("requires two IOSurface-backed planes".into()));
        }
        let formats = plane_formats(format);
        let metals = match format {
            PixelFormat::Nv12 => [MTLPixelFormat::R8Unorm, MTLPixelFormat::RG8Unorm],
            PixelFormat::P010 => [MTLPixelFormat::R16Unorm, MTLPixelFormat::RG16Unorm],
        };
        let plane = |index: usize, size: [u32; 2]| -> Result<wgpu::Texture, Error> {
            if CVPixelBufferGetWidthOfPlane(buffer, index) != size[0] as usize
                || CVPixelBufferGetHeightOfPlane(buffer, index) != size[1] as usize
            {
                return Err(Error::Frame("unexpected chroma subsampling"));
            }
            let mut raw = null_mut();
            // SAFETY: Valid retained cache and borrowed buffer, validated plane index and
            // dimensions. Format matches the fourcc's storage; output pointer lives through call.
            check(unsafe {
                CVMetalTextureCache::create_texture_from_image(
                    None,
                    &self.cache,
                    buffer,
                    None,
                    metals[index],
                    size[0] as usize,
                    size[1] as usize,
                    index,
                    NonNull::from(&mut raw),
                )
            })?;
            let raw = NonNull::new(raw).ok_or_else(|| Error::Import("null CVMetalTexture".into()))?;
            // SAFETY: The successful Create returned one owned CF reference, adopted once.
            let cv = unsafe { CFRetained::from_raw(raw) };
            let texture = CVMetalTextureGetTexture(&cv).ok_or_else(|| Error::Import("null MTLTexture".into()))?;
            // SAFETY: buffer is live for this borrow; add our own retain until the HAL texture
            // is released, preventing decoder pool reuse even after the caller drops its frame.
            let retained = unsafe { CFRetained::retain(NonNull::from(buffer)) };
            let owner = PlaneOwner { _texture: cv, _buffer: retained };
            let desc = descriptor(size, formats[index], false);
            // SAFETY: CV cache uses exactly this wgpu Metal device; its plane format, dimensions,
            // single mip/layer and 2D type match desc. The callback owns both the CV mapping and
            // pixel buffer until wgpu has released all in-flight uses; no external texture destroy.
            let hal = unsafe {
                wgpu::hal::metal::Device::texture_from_raw(
                    texture,
                    formats[index],
                    MTLTextureType::Type2D,
                    1,
                    1,
                    wgpu::hal::CopyExtent { width: size[0], height: size[1], depth: 1 },
                    Some(Box::new(move || drop(owner))),
                )
            };
            // SAFETY: Same-device texture, exact descriptor above, initialized by the decoder.
            // Only sampled access is exposed. Metal's resource state is RESOURCE for sampling.
            Ok(unsafe {
                self.device.create_texture_from_hal::<wgpu::hal::api::Metal>(hal, &desc, wgpu::TextureUses::RESOURCE)
            })
        };
        let planes = [plane(0, [width, height])?, plane(1, [width / 2, height / 2])?];
        // Release unused cache entries; live plane owners above keep their mappings alive.
        self.cache.flush(0);
        Ok(Frame { planes, format, color })
    }
}

fn check(status: i32) -> Result<(), Error> {
    if status == 0 { Ok(()) } else { Err(Error::Import(format!("CoreVideo status {status}"))) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{Fixture, Gpu, compare, render, target};
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFString, CFType};

    fn pixel_buffer(f: &Fixture) -> CFRetained<CVPixelBuffer> {
        let empty = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        // SAFETY: CoreVideo constants are immutable system CFStrings. Values have the exact
        // types required by these keys (dictionary and CFBoolean), retained by the dictionary.
        let attrs = unsafe {
            CFDictionary::<CFString, CFType>::from_slices(
                &[kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferMetalCompatibilityKey],
                &[AsRef::<CFType>::as_ref(&*empty), CFBoolean::new(true).as_ref()],
            )
        };
        let fourcc = match (f.format, f.color.range) {
            (PixelFormat::Nv12, Range::Limited) => kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            (PixelFormat::Nv12, Range::Full) => kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            (PixelFormat::P010, Range::Limited) => kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            (PixelFormat::P010, Range::Full) => kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
        };
        let mut raw = null_mut();
        // SAFETY: Bounded even dimensions, correctly typed attrs above and valid out pointer.
        check(unsafe {
            CVPixelBufferCreate(
                None,
                f.size[0] as usize,
                f.size[1] as usize,
                fourcc,
                Some(attrs.as_opaque()),
                NonNull::from(&mut raw),
            )
        })
        .unwrap();
        // SAFETY: Successful Create returned a new +1 CF reference, adopted once.
        let b = unsafe { CFRetained::from_raw(NonNull::new(raw).unwrap()) };
        // SAFETY: This is a freshly allocated buffer with no aliases or GPU users.
        check(unsafe { CVPixelBufferLockBaseAddress(&b, CVPixelBufferLockFlags(0)) }).unwrap();
        let row_bytes = f.size[0] as usize * if f.format == PixelFormat::P010 { 2 } else { 1 };
        for plane in 0..2 {
            let ptr = CVPixelBufferGetBaseAddressOfPlane(&b, plane).cast::<u8>();
            let stride = CVPixelBufferGetBytesPerRowOfPlane(&b, plane);
            assert!(!ptr.is_null() && stride >= row_bytes);
            let rows = f.size[1] as usize / (plane + 1);
            for row in 0..rows {
                let bytes = &f.planes[plane][row * f.strides[plane] as usize..][..row_bytes];
                // SAFETY: Locked plane, checked stride and dimensions, source slice bounds
                // checked above. Independent freshly allocated destination cannot overlap.
                unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.add(row * stride), row_bytes) };
            }
        }
        // SAFETY: Balances the successful lock; no further CPU writes occur after publication.
        check(unsafe { CVPixelBufferUnlockBaseAddress(&b, CVPixelBufferLockFlags(0)) }).unwrap();
        b
    }

    #[test]
    fn iosurface_matches_cpu_and_survives_dropped_owners() {
        let gpu = Gpu::new().unwrap();
        let mut renderer = Renderer::new(&gpu.device);
        let mut count = 0;
        let mut worst = 0;
        for format in [PixelFormat::Nv12, PixelFormat::P010] {
            for range in [Range::Limited, Range::Full] {
                for transfer in [Transfer::Bt1886, Transfer::Pq, Transfer::Hlg] {
                    let color = Color {
                        range,
                        transfer,
                        matrix: Matrix::Bt2020,
                        primaries: Primaries::Bt2020,
                        ..Color::SDR709
                    };
                    let fixture = Fixture::new([32, 16], format, color);
                    let buffer = pixel_buffer(&fixture);
                    // Deliberately wrong range in caller metadata: import must use the fourcc.
                    let imported = renderer
                        .import(
                            &buffer,
                            Color { range: if range == Range::Full { Range::Limited } else { Range::Full }, ..color },
                        )
                        .unwrap();
                    let cpu = renderer.upload(&gpu.queue, &fixture.frame()).unwrap();
                    for srgb in [false, true] {
                        let t = target(srgb);
                        let expected = render(&gpu, renderer.prepare(&cpu, t).unwrap(), t);
                        let actual = render(&gpu, renderer.prepare(&imported, t).unwrap(), t);
                        worst = worst.max(compare(&actual, &expected, 1));
                        count += 1;
                    }
                    let t = target(true);
                    let expected = render(&gpu, renderer.prepare(&cpu, t).unwrap(), t);
                    let draw = renderer.prepare(&imported, t).unwrap();
                    drop(imported);
                    drop(buffer);
                    // Drop the cache too: only the draw/command's HAL owners keep planes alive.
                    renderer.importer = None;
                    let actual = render(&gpu, draw, t);
                    compare(&actual, &expected, 1);
                }
            }
        }
        println!("{count} IOSurface/CPU images, worst {worst}/255; 12 drop-before-submit cases");
    }
}
