use super::*;

pub(super) struct Gpu {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    name: String,
}

impl Gpu {
    pub fn new() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let adapter = match pollster::block_on(instance.request_adapter(&Default::default())) {
            Ok(a) => a,
            Err(e) => {
                if cfg!(target_os = "macos") {
                    panic!("macOS requires a real GPU: {e}");
                }
                eprintln!("GPU tests skipped: {e}");
                return None;
            }
        };
        let features = P010_FEATURES | (adapter.features() & wgpu::Features::TIMESTAMP_QUERY);
        let (device, queue) = pollster::block_on(
            adapter.request_device(&wgpu::DeviceDescriptor { required_features: features, ..Default::default() }),
        )
        .unwrap();
        Some(Self { device, queue, name: adapter.get_info().name })
    }
}

pub(super) struct Fixture {
    pub size: [u32; 2],
    pub format: PixelFormat,
    pub color: Color,
    pub planes: [Vec<u8>; 2],
    pub strides: [u32; 2],
}

impl Fixture {
    pub fn new(size: [u32; 2], format: PixelFormat, color: Color) -> Self {
        let bytes = if format == PixelFormat::P010 { 2 } else { 1 };
        let strides = [size[0] * bytes + 8, size[0] * bytes + 16];
        let mut planes = [vec![0; (strides[0] * size[1]) as usize], vec![0; (strides[1] * size[1] / 2) as usize]];
        for plane in 0..2 {
            let rows = size[1] / if plane == 0 { 1 } else { 2 };
            for y in 0..rows {
                for x in 0..size[0] {
                    // Includes below-black, above-white, saturated colours, neutral ramps and
                    // nonuniform UV. Padding stays zero to expose accidental stride sampling.
                    let code = if plane == 0 {
                        ((x * 251 / size[0] + y * 7) % 256) as u16
                    } else if y < rows / 2 {
                        128
                    } else {
                        (72 + (x * 13 + y * 17) % 113) as u16
                    };
                    let at = (y * strides[plane] + x * bytes) as usize;
                    if bytes == 1 {
                        planes[plane][at] = code as u8;
                    } else {
                        planes[plane][at..at + 2].copy_from_slice(&((code * 4 + (x % 4) as u16) << 6).to_le_bytes());
                    }
                }
            }
        }
        // Deliberately no padding after the last active row.
        for i in 0..2 {
            planes[i].truncate((strides[i] * (size[1] / (i as u32 + 1) - 1) + size[0] * bytes) as usize);
        }
        Self { size, format, color, planes, strides }
    }
    pub fn frame(&self) -> CpuFrame<'_> {
        CpuFrame {
            size: self.size,
            format: self.format,
            color: self.color,
            planes: [&self.planes[0], &self.planes[1]],
            strides: self.strides,
        }
    }
}

fn read_buffer(device: &wgpu::Device, buffer: &wgpu::Buffer) -> Vec<u8> {
    let (tx, rx) = std::sync::mpsc::channel();
    buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| tx.send(r).unwrap());
    device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
    rx.recv().unwrap().unwrap();
    let bytes = buffer.slice(..).get_mapped_range().unwrap().to_vec();
    buffer.unmap();
    bytes
}

pub(super) fn render(gpu: &Gpu, draw: Draw, target: Target) -> Vec<u8> {
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("golden target"),
        size: wgpu::Extent3d { width: target.size[0], height: target.size[1], depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: target.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let pixel_bytes = target.format.block_copy_size(None).unwrap();
    let row_bytes = target.size[0] * pixel_bytes;
    let stride = row_bytes.div_ceil(256) * 256;
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: u64::from(stride) * u64::from(target.size[1]),
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    {
        let view = texture.create_view(&Default::default());
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        draw.record(&mut pass);
    }
    // Test resource lifetime: keep neither Draw nor Frame alive until submission.
    drop(draw);
    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(stride), rows_per_image: None },
        },
        texture.size(),
    );
    gpu.queue.submit([encoder.finish()]);
    let bytes: Vec<u8> = read_buffer(&gpu.device, &readback)
        .chunks(stride as usize)
        .flat_map(|row| &row[..row_bytes as usize])
        .copied()
        .collect();
    if target.format == wgpu::TextureFormat::Rgba16Float {
        // Quantize read-back half-floats for comparison with the same 8-bit reference images.
        bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| {
                let bits = u16::from_le_bytes([b[0], b[1]]);
                assert_eq!(bits & 0x8000, 0, "negative video output");
                let exponent = (bits >> 10) & 31;
                assert_ne!(exponent, 31, "nonfinite video output");
                let mantissa = f64::from(bits & 1023);
                let value = if exponent == 0 {
                    mantissa * 2_f64.powi(-24)
                } else {
                    (1.0 + mantissa / 1024.0) * 2_f64.powi(i32::from(exponent) - 15)
                };
                (value * 255.0).round() as u8
            })
            .collect()
    } else if matches!(target.format, wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb) {
        bytes.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect()
    } else {
        bytes
    }
}

pub(super) fn target(srgb_blend: bool) -> Target {
    Target {
        format: if srgb_blend { wgpu::TextureFormat::Rgba8Unorm } else { wgpu::TextureFormat::Rgba8UnormSrgb },
        size: [48, 24],
        rect: [-2.0, 2.0, 46.0, 20.0],
        srgb_blend,
    }
}

// Scalar f64 reference, independent of the GPU uniforms, sampling and shader. The golden image
// is generated in memory, never by accepting the renderer's own output as its expected image.
// PQ/BT.2390 equations adapted from FilmCraft crates/color; see THIRD_PARTY_NOTICES.
fn pq_encode(nits: f64) -> f64 {
    let p = (nits.clamp(0.0, 10000.0) / 10000.0).powf(2610.0 / 16384.0);
    ((3424.0 / 4096.0 + 2413.0 / 128.0 * p) / (1.0 + 2392.0 / 128.0 * p)).powf(2523.0 / 32.0)
}
fn pq_decode(e: f64) -> f64 {
    let p = e.max(0.0).powf(32.0 / 2523.0);
    ((p - 3424.0 / 4096.0).max(0.0) / (2413.0 / 128.0 - 2392.0 / 128.0 * p)).powf(16384.0 / 2610.0) * 10000.0
}
fn tone(nits: f64, peak: f64) -> f64 {
    if peak <= 203.0 {
        return nits.min(203.0);
    }
    let maximum = pq_encode(peak);
    let x = (pq_encode(nits) / maximum).min(1.0);
    let end = pq_encode(203.0) / maximum;
    let knee = 1.5 * end - 0.5;
    if x < knee {
        return nits;
    }
    let t = (x - knee) / (1.0 - knee);
    let value = (2.0 * t.powi(3) - 3.0 * t.powi(2) + 1.0) * knee
        + (t.powi(3) - 2.0 * t.powi(2) + t) * (1.0 - knee)
        + (-2.0 * t.powi(3) + 3.0 * t.powi(2)) * end;
    pq_decode(value * maximum)
}
fn srgb(v: f64) -> f64 {
    if v <= 0.0031308 { 12.92 * v } else { 1.055 * v.powf(1.0 / 2.4) - 0.055 }
}

fn reference_color(code: [f64; 3], format: PixelFormat, color: Color) -> [f64; 3] {
    let factor = if format == PixelFormat::P010 { 4.0 } else { 1.0 };
    let max = if format == PixelFormat::P010 { 1023.0 } else { 255.0 };
    let (y, cb, cr) = match color.range {
        Range::Limited => {
            ((code[0] / factor - 16.0) / 219.0, (code[1] / factor - 128.0) / 224.0, (code[2] / factor - 128.0) / 224.0)
        }
        Range::Full => (code[0] / max, (code[1] - 128.0 * factor) / max, (code[2] - 128.0 * factor) / max),
    };
    let (kr, kb) = match color.matrix {
        Matrix::Bt601 => (0.299, 0.114),
        Matrix::Bt709 => (0.2126, 0.0722),
        Matrix::Bt2020 => (0.2627, 0.0593),
    };
    let kg = 1.0 - kr - kb;
    let mut c = [
        y + (2.0 - 2.0 * kr) * cr,
        y - 2.0 * kb * (1.0 - kb) / kg * cb - 2.0 * kr * (1.0 - kr) / kg * cr,
        y + (2.0 - 2.0 * kb) * cb,
    ];
    c = c.map(|v| {
        let v = v.clamp(0.0, 1.0);
        match color.transfer {
            Transfer::Bt1886 => v.powf(2.4),
            Transfer::Srgb => {
                if v <= 0.04045 {
                    v / 12.92
                } else {
                    ((v + 0.055) / 1.055).powf(2.4)
                }
            }
            Transfer::Pq => pq_decode(v) / 203.0,
            Transfer::Hlg => {
                if v <= 0.5 {
                    v * v / 3.0
                } else {
                    (((v - 0.55991073) / 0.17883277).exp() + 0.28466892) / 12.0
                }
            }
        }
    });
    if color.transfer == Transfer::Hlg {
        let l = if color.primaries == Primaries::Bt2020 { [0.2627, 0.6780, 0.0593] } else { [0.2126, 0.7152, 0.0722] };
        let lum = c.iter().zip(l).map(|(c, w)| c * w).sum::<f64>();
        c = c.map(|v| v * lum.powf(0.2) * 1000.0 / 203.0);
    }
    if color.primaries == Primaries::Bt2020 {
        let m = [[1.660491, -0.587641, -0.072850], [-0.124550, 1.132900, -0.008349], [-0.018151, -0.100579, 1.118730]];
        c = m.map(|row| row.iter().zip(c).map(|(a, b)| a * b).sum());
    }
    if matches!(color.transfer, Transfer::Pq | Transfer::Hlg) {
        let max = c.into_iter().fold(0.0_f64, f64::max);
        let peak = if color.transfer == Transfer::Hlg { 1000.0 } else { color.peak_nits as f64 };
        if max > 0.0 {
            c = c.map(|v| v * tone(max * 203.0, peak) / (max * 203.0));
        }
    }
    let min = c.into_iter().fold(f64::INFINITY, f64::min);
    if min < 0.0 {
        let lum = c[0] * 0.2126 + c[1] * 0.7152 + c[2] * 0.0722;
        c = if lum <= 0.0 { [0.0; 3] } else { c.map(|v| lum + (v - lum) * lum / (lum - min)) };
    }
    c.map(|v| v.clamp(0.0, 1.0))
}

fn sample(f: &Fixture, plane: usize, channel: usize, uv: [f64; 2]) -> f64 {
    let divisor = if plane == 0 { 1 } else { 2 };
    let [w, h] = f.size.map(|v| v / divisor);
    let [x, y] = [uv[0] * w as f64 - 0.5, uv[1] * h as f64 - 0.5];
    let load = |px: f64, py: f64| {
        let px = px.clamp(0.0, (w - 1) as f64) as usize;
        let py = py.clamp(0.0, (h - 1) as f64) as usize;
        let offset = py * f.strides[plane] as usize
            + (px * divisor as usize + channel) * if f.format == PixelFormat::P010 { 2 } else { 1 };
        if f.format == PixelFormat::Nv12 {
            f.planes[plane][offset] as f64
        } else {
            (u16::from_le_bytes([f.planes[plane][offset], f.planes[plane][offset + 1]]) >> 6) as f64
        }
    };
    let (fx, fy) = (x - x.floor(), y - y.floor());
    load(x.floor(), y.floor()) * (1.0 - fx) * (1.0 - fy)
        + load(x.floor() + 1.0, y.floor()) * fx * (1.0 - fy)
        + load(x.floor(), y.floor() + 1.0) * (1.0 - fx) * fy
        + load(x.floor() + 1.0, y.floor() + 1.0) * fx * fy
}

fn golden(f: &Fixture, t: Target) -> Vec<u8> {
    let mut pixels = Vec::new();
    for y in 0..t.size[1] {
        for x in 0..t.size[0] {
            let uv = [
                (x as f64 + 0.5 - t.rect[0] as f64) / t.rect[2] as f64,
                (y as f64 + 0.5 - t.rect[1] as f64) / t.rect[3] as f64,
            ];
            if uv.iter().any(|v| *v < 0.0 || *v >= 1.0) {
                pixels.extend([0; 4]);
                continue;
            }
            let code = [sample(f, 0, 0, uv), sample(f, 1, 0, uv), sample(f, 1, 1, uv)];
            let rgb = reference_color(code, f.format, f.color);
            pixels.extend(rgb.map(|v| {
                let v = if t.srgb_blend || t.format.is_srgb() { srgb(v) } else { v };
                (v * 255.0).round() as u8
            }));
            pixels.push(255);
        }
    }
    pixels
}

pub(super) fn compare(actual: &[u8], expected: &[u8], tolerance: u8) -> u8 {
    assert_eq!(actual.len(), expected.len());
    let (at, worst) =
        actual.iter().zip(expected).enumerate().map(|(i, (a, b))| (i, a.abs_diff(*b))).max_by_key(|x| x.1).unwrap();
    assert!(
        worst <= tolerance,
        "byte {at}: actual {}, expected {}, max error {worst} > {tolerance}",
        actual[at],
        expected[at]
    );
    worst
}

#[test]
fn generated_golden_images() {
    let Some(gpu) = Gpu::new() else { return };
    let mut renderer = Renderer::new(&gpu.device);
    let mut worst = 0;
    let mut count = 0;
    for format in [PixelFormat::Nv12, PixelFormat::P010] {
        for matrix in [Matrix::Bt601, Matrix::Bt709, Matrix::Bt2020] {
            for range in [Range::Limited, Range::Full] {
                for transfer in [Transfer::Bt1886, Transfer::Srgb, Transfer::Pq, Transfer::Hlg] {
                    let color = Color {
                        matrix,
                        range,
                        transfer,
                        primaries: if matrix == Matrix::Bt2020 { Primaries::Bt2020 } else { Primaries::Bt709 },
                        ..Color::SDR709
                    };
                    let fixture = Fixture::new([32, 16], format, color);
                    let frame = renderer.upload(&gpu.queue, &fixture.frame()).unwrap();
                    for mode in 0..7 {
                        let mut t = target(matches!(mode, 0 | 3 | 5));
                        if mode == 2 {
                            t.format = wgpu::TextureFormat::Rgba8Unorm;
                        }
                        if mode == 3 {
                            t.format = wgpu::TextureFormat::Bgra8Unorm;
                        }
                        if mode == 4 {
                            t.format = wgpu::TextureFormat::Bgra8UnormSrgb;
                        }
                        if mode >= 5 {
                            t.format = wgpu::TextureFormat::Rgba16Float;
                        }
                        let draw = renderer.prepare(&frame, t).unwrap();
                        let pixels = render(&gpu, draw, t);
                        worst = worst.max(compare(&pixels, &golden(&fixture, t), 2));
                        count += 1;
                    }
                }
            }
        }
    }
    println!(
        "{count} generated golden images (NV12/P010 × 601/709/2020 × limited/full × SDR/sRGB/PQ/HLG × encoded/linear modes), worst {worst}/255 on {}",
        gpu.name
    );
}

#[test]
fn invalid_frames_and_targets_return_errors() {
    let Some(gpu) = Gpu::new() else { return };
    let mut renderer = Renderer::new(&gpu.device);
    let fixture = Fixture::new([32, 16], PixelFormat::P010, Color::SDR709);
    for size in [[0, 16], [31, 16], [32, 15], [u32::MAX, 2], [8194, 8194]] {
        let mut bad = fixture.frame();
        bad.size = size;
        assert!(renderer.upload(&gpu.queue, &bad).is_err());
    }
    for plane in 0..2 {
        let mut bad = fixture.frame();
        bad.planes[plane] = &fixture.planes[plane][..3];
        assert!(renderer.upload(&gpu.queue, &bad).is_err());
        let mut bad = fixture.frame();
        bad.strides[plane] = u32::MAX;
        assert!(renderer.upload(&gpu.queue, &bad).is_err());
    }
    for peak in [f32::NAN, f32::INFINITY, -1.0, 10001.0] {
        let mut bad = fixture.frame();
        bad.color.peak_nits = peak;
        assert!(renderer.upload(&gpu.queue, &bad).is_err());
    }
    let frame = renderer.upload(&gpu.queue, &fixture.frame()).unwrap();
    for rect in [[0.0, 0.0, 0.0, 1.0], [f32::NAN, 0.0, 1.0, 1.0], [0.0, 0.0, -1.0, 1.0]] {
        assert!(renderer.prepare(&frame, Target { rect, ..target(true) }).is_err());
    }
    assert!(renderer.prepare(&frame, Target { srgb_blend: true, ..target(false) }).is_err());
}

#[test]
#[ignore = "manual GPU timestamp benchmark; needs an idle Metal GPU"]
fn p010_4k_gpu_time() {
    let gpu = Gpu::new().unwrap();
    assert!(gpu.device.features().contains(wgpu::Features::TIMESTAMP_QUERY));
    let mut renderer = Renderer::new(&gpu.device);
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: 3840, height: 2160, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());
    let queries = gpu.device.create_query_set(&wgpu::QuerySetDescriptor {
        label: None,
        ty: wgpu::QueryType::Timestamp,
        count: 2,
    });
    let resolve = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: 16,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let mut medians = Vec::new();
    for transfer in [Transfer::Bt1886, Transfer::Pq, Transfer::Hlg] {
        let color = Color { matrix: Matrix::Bt2020, primaries: Primaries::Bt2020, transfer, ..Color::SDR709 };
        let fixture = Fixture::new([3840, 2160], PixelFormat::P010, color);
        let frame = renderer.upload(&gpu.queue, &fixture.frame()).unwrap();
        let draw = renderer
            .prepare(
                &frame,
                Target {
                    format: texture.format(),
                    size: [3840, 2160],
                    rect: [0.0, 0.0, 3840.0, 2160.0],
                    srgb_blend: true,
                },
            )
            .unwrap();
        gpu.queue.submit([]);
        gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        let mut timings = Vec::new();
        for run in 0..72 {
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            {
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    timestamp_writes: Some(wgpu::RenderPassTimestampWrites {
                        query_set: &queries,
                        beginning_of_pass_write_index: Some(0),
                        end_of_pass_write_index: Some(1),
                    }),
                    ..Default::default()
                });
                draw.record(&mut pass);
            }
            // On this Metal driver, resolving stage counters in the producing command buffer
            // can return the previous pass's end counter. Resolve only after GPU completion;
            // the timestamps still bracket only the render pass, not this CPU wait.
            gpu.queue.submit([encoder.finish()]);
            gpu.device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
            let mut encoder = gpu.device.create_command_encoder(&Default::default());
            encoder.resolve_query_set(&queries, 0..2, &resolve, 0);
            encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, 16);
            gpu.queue.submit([encoder.finish()]);
            let bytes = read_buffer(&gpu.device, &readback);
            let start = u64::from_ne_bytes(bytes[..8].try_into().unwrap());
            let end = u64::from_ne_bytes(bytes[8..].try_into().unwrap());
            assert!(end > start, "invalid GPU timestamp pair: {start}, {end}");
            if run >= 8 {
                timings.push((end - start) as f64 * gpu.queue.get_timestamp_period() as f64 / 1e6);
            }
        }
        timings.sort_by(f64::total_cmp);
        println!(
            "{}: 3840×2160 P010 {transfer:?}, 64 draws after 8 warmup; median {:.4} ms, p95 {:.4} ms, max {:.4} ms",
            gpu.name, timings[32], timings[60], timings[63]
        );
        medians.push(timings[32]);
    }
    assert!(medians.iter().all(|v| *v < 1.0), "4K steady-state median must be < 1 ms");
}
