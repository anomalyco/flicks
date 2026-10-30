//! NV12/P010 → SDR in one pass, directly into the host's render target (no RGBA intermediate).
//!
//! Request [`P010_FEATURES`] on the host device for 10-bit frames. Upload/import before drawing;
//! call [`Renderer::prepare`] with the effect site's format, size, encoding and target-local
//! pixel rectangle, then [`Draw::record`] inside the host pass. A prepared draw is immutable:
//! several draws recorded before submission never overwrite each other's uniforms. The host
//! owns submission, clipping/scissor, opacity layers and aspect-ratio policy. Single-sample
//! colour passes only. A Rune compositor uses `site.instance.rect - site.origin` and
//! `site.srgb_blend`; this crate deliberately has no Rune dependency.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use std::collections::HashMap;
use wgpu::util::DeviceExt;

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;
#[cfg(test)]
mod tests;

pub const P010_FEATURES: wgpu::Features = wgpu::Features::TEXTURE_FORMAT_16BIT_NORM;
const MAX_DIMENSION: u32 = 8192;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid frame: {0}")]
    Frame(&'static str),
    #[error("invalid draw target or rectangle")]
    Target,
    #[error("P010 needs TEXTURE_FORMAT_16BIT_NORM on the host device")]
    P010Feature,
    #[error("CoreVideo/Metal import: {0}")]
    Import(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
    /// Little-endian 16-bit words; ten significant bits in bits 15..6, as in VideoToolbox.
    P010,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
    /// BT.2020 non-constant luminance.
    Bt2020,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Limited,
    Full,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transfer {
    /// SDR display-referred video, BT.1886 with zero black (gamma 2.4).
    Bt1886,
    Srgb,
    Pq,
    Hlg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Primaries {
    Bt709,
    Bt2020,
}

/// Matrix and primaries are independent (H.273 metadata). BT.601 matrix material normally
/// uses the BT.709 display gamut here. HDR is mapped to a 203 cd/m² reference-white SDR display.
#[derive(Clone, Copy, Debug)]
pub struct Color {
    pub matrix: Matrix,
    pub range: Range,
    pub transfer: Transfer,
    pub primaries: Primaries,
    /// PQ mastering/content peak; use 1000 when absent. HLG uses its nominal 1000-nit display.
    pub peak_nits: f32,
}

impl Color {
    pub const SDR709: Self = Self {
        matrix: Matrix::Bt709,
        range: Range::Limited,
        transfer: Transfer::Bt1886,
        primaries: Primaries::Bt709,
        peak_nits: 1000.0,
    };
}

/// Bi-planar 4:2:0 data, with explicit byte strides. Padding is allowed; the final row need
/// only contain its active pixels. Chroma is centre-sited and reconstructed bilinearly.
pub struct CpuFrame<'a> {
    /// Coded size, including any padding rows and columns the decoder adds (1920×1088).
    pub size: [u32; 2],
    /// The visible rectangle (x, y, width, height in luma samples) within `size`: only it is
    /// drawn, and sampling never reads outside it.
    pub crop: [u32; 4],
    pub format: PixelFormat,
    pub planes: [&'a [u8]; 2],
    pub strides: [u32; 2],
    pub color: Color,
}

/// Owned GPU planes. Imported planes retain the CVPixelBuffer through GPU completion,
/// including when this handle is dropped before submission.
pub struct Frame {
    planes: [wgpu::Texture; 2],
    format: PixelFormat,
    color: Color,
    crop: [u32; 4],
}

impl Frame {
    pub fn size(&self) -> [u32; 2] {
        [self.planes[0].width(), self.planes[0].height()]
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Target {
    pub format: wgpu::TextureFormat,
    pub size: [u32; 2],
    /// Target-local x, y, width, height, in pixels. Negative origins are allowed.
    pub rect: [f32; 4],
    /// True: write sRGB-encoded values to a non-sRGB attachment. False: write linear values
    /// (the host normally uses an *Srgb attachment, or Rgba16Float for an isolated layer).
    pub srgb_blend: bool,
}

pub struct Renderer {
    device: wgpu::Device,
    shader: wgpu::ShaderModule,
    layout: wgpu::BindGroupLayout,
    pipelines: HashMap<wgpu::TextureFormat, wgpu::RenderPipeline>,
    #[cfg(target_os = "macos")]
    importer: Option<macos::Importer>,
}

pub struct Draw {
    pipeline: wgpu::RenderPipeline,
    bindings: wgpu::BindGroup,
}

impl Draw {
    /// Does not change the host's viewport or scissor. The viewport must cover the target.
    pub fn record(&self, pass: &mut wgpu::RenderPass<'_>) {
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bindings, &[]);
        pass.draw(0..6, 0..1);
    }
}

impl Renderer {
    pub fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("Flicks video"),
            source: wgpu::ShaderSource::Wgsl(include_str!("video.wgsl").into()),
        });
        let texture = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("Flicks video"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(96),
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: texture,
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: texture,
                    count: None,
                },
            ],
        });
        Self {
            device: device.clone(),
            shader,
            layout,
            pipelines: HashMap::new(),
            #[cfg(target_os = "macos")]
            importer: None,
        }
    }

    pub fn upload(&self, queue: &wgpu::Queue, frame: &CpuFrame<'_>) -> Result<Frame, Error> {
        validate_size(&self.device, frame.size, frame.format)?;
        validate_crop(frame.size, frame.crop)?;
        validate_color(frame.color)?;
        let row_bytes = frame.size[0] * if frame.format == PixelFormat::P010 { 2 } else { 1 };
        for (i, rows) in [frame.size[1], frame.size[1] / 2].into_iter().enumerate() {
            let required = u64::from(rows - 1) * u64::from(frame.strides[i]) + u64::from(row_bytes);
            if frame.strides[i] < row_bytes || required > frame.planes[i].len() as u64 {
                return Err(Error::Frame("short plane or stride"));
            }
            let texel_bytes = (if frame.format == PixelFormat::P010 { 2 } else { 1 }) * (i as u32 + 1);
            if !frame.strides[i].is_multiple_of(texel_bytes) {
                return Err(Error::Frame("stride must be aligned to a plane texel"));
            }
        }
        let formats = plane_formats(frame.format);
        let planes = std::array::from_fn(|i| {
            let divisor = if i == 0 { 1 } else { 2 };
            let desc = descriptor([frame.size[0] / divisor, frame.size[1] / divisor], formats[i], true);
            let texture = self.device.create_texture(&desc);
            queue.write_texture(
                texture.as_image_copy(),
                frame.planes[i],
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(frame.strides[i]), rows_per_image: None },
                desc.size,
            );
            texture
        });
        Ok(Frame { planes, format: frame.format, color: frame.color, crop: frame.crop })
    }

    /// Import a completed IOSurface-backed VideoToolbox frame without locking or copying pixels.
    /// The caller must not recycle or modify the pixels until GPU work completes. The import
    /// retains the buffer (preventing CVPixelBufferPool reuse); no dependency on the decoder.
    /// Range comes from the buffer's fourcc; matrix/transfer/primaries come from stream metadata.
    /// `crop` is the visible rectangle as in [`CpuFrame::crop`].
    #[cfg(target_os = "macos")]
    pub fn import(
        &mut self,
        buffer: &objc2_core_video::CVPixelBuffer,
        color: Color,
        crop: [u32; 4],
    ) -> Result<Frame, Error> {
        validate_color(color)?;
        if self.importer.is_none() {
            self.importer = Some(macos::Importer::new(&self.device)?);
        }
        self.importer.as_ref().ok_or_else(|| Error::Import("missing cache".into()))?.import(buffer, color, crop)
    }

    pub fn prepare(&mut self, frame: &Frame, target: Target) -> Result<Draw, Error> {
        use wgpu::TextureFormat as F;
        if target.size.contains(&0)
            || target.size.iter().any(|v| *v > self.device.limits().max_texture_dimension_2d)
            || target.rect.iter().any(|v| !v.is_finite() || v.abs() > 1e7)
            || target.rect[2] <= 0.0
            || target.rect[3] <= 0.0
            || (target.srgb_blend && target.format.is_srgb())
            || !matches!(
                target.format,
                F::Rgba8Unorm | F::Rgba8UnormSrgb | F::Bgra8Unorm | F::Bgra8UnormSrgb | F::Rgba16Float
            )
        {
            return Err(Error::Target);
        }
        let pipeline = self.pipelines.entry(target.format).or_insert_with(|| {
            let layout = self.device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Flicks video"),
                bind_group_layouts: &[Some(&self.layout)],
                immediate_size: 0,
            });
            self.device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("Flicks video"),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &self.shader,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                fragment: Some(wgpu::FragmentState {
                    module: &self.shader,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: target.format,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                multiview_mask: None,
                cache: None,
            })
        });
        let color = frame.color;
        let (kr, kb) = match color.matrix {
            Matrix::Bt601 => (0.299, 0.114),
            Matrix::Bt709 => (0.2126, 0.0722),
            Matrix::Bt2020 => (0.2627, 0.0593),
        };
        let ten = frame.format == PixelFormat::P010;
        let scale = if ten { 4.0 } else { 1.0 };
        let max = if ten { 1023.0 } else { 255.0 };
        let (yoff, yrange, crange) = match color.range {
            Range::Limited => (16.0 * scale, 219.0 * scale, 224.0 * scale),
            Range::Full => (0.0, max, max),
        };
        let transfer = match color.transfer {
            Transfer::Bt1886 => 0.0,
            Transfer::Srgb => 1.0,
            Transfer::Pq => 2.0,
            Transfer::Hlg => 3.0,
        };
        let uniforms: [[f32; 4]; 6] = [
            target.rect,
            [target.size[0] as f32, target.size[1] as f32, f32::from(target.srgb_blend), 0.0],
            [yoff, yrange, 128.0 * scale, crange],
            [kr, kb, if ten { 65535.0 / 64.0 } else { 255.0 }, transfer],
            [
                f32::from(color.primaries == Primaries::Bt2020),
                if color.transfer == Transfer::Hlg { 1000.0 } else { color.peak_nits },
                0.0,
                0.0,
            ],
            frame.crop.map(|v| v as f32),
        ];
        let uniform = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Flicks draw"),
            contents: bytemuck::cast_slice(&uniforms),
            usage: wgpu::BufferUsages::UNIFORM,
        });
        let views = frame.planes.each_ref().map(|p| p.create_view(&Default::default()));
        let bindings = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("Flicks draw"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&views[0]) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&views[1]) },
            ],
        });
        Ok(Draw { pipeline: pipeline.clone(), bindings })
    }
}

fn validate_size(device: &wgpu::Device, size: [u32; 2], format: PixelFormat) -> Result<(), Error> {
    if size
        .iter()
        .any(|v| *v == 0 || *v > MAX_DIMENSION.min(device.limits().max_texture_dimension_2d) || !v.is_multiple_of(2))
    {
        return Err(Error::Frame("4:2:0 requires even dimensions, 2..8192 within device limits"));
    }
    if format == PixelFormat::P010 && !device.features().contains(P010_FEATURES) {
        return Err(Error::P010Feature);
    }
    Ok(())
}

fn validate_crop(size: [u32; 2], crop: [u32; 4]) -> Result<(), Error> {
    let [x, y, w, h] = crop;
    if w == 0 || h == 0 || x.checked_add(w).is_none_or(|r| r > size[0]) || y.checked_add(h).is_none_or(|b| b > size[1])
    {
        return Err(Error::Frame("crop must be a non-empty rectangle within the frame"));
    }
    Ok(())
}

fn validate_color(color: Color) -> Result<(), Error> {
    if !color.peak_nits.is_finite() || !(203.0..=10000.0).contains(&color.peak_nits) {
        return Err(Error::Frame("peak must be finite and in 203..10000 nits"));
    }
    Ok(())
}

fn plane_formats(format: PixelFormat) -> [wgpu::TextureFormat; 2] {
    match format {
        PixelFormat::Nv12 => [wgpu::TextureFormat::R8Unorm, wgpu::TextureFormat::Rg8Unorm],
        PixelFormat::P010 => [wgpu::TextureFormat::R16Unorm, wgpu::TextureFormat::Rg16Unorm],
    }
}

fn descriptor(size: [u32; 2], format: wgpu::TextureFormat, upload: bool) -> wgpu::TextureDescriptor<'static> {
    wgpu::TextureDescriptor {
        label: Some("Flicks Y/UV plane"),
        size: wgpu::Extent3d { width: size[0], height: size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING
            | if upload { wgpu::TextureUsages::COPY_DST } else { wgpu::TextureUsages::empty() },
        view_formats: &[],
    }
}
