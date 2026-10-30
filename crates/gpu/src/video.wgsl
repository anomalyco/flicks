// YCbCr and HDR colour math adapted from FilmCraft fed0f42, crates/gpu/composite.wgsl
// and crates/color/transform.rs (MIT OR Apache-2.0). See THIRD_PARTY_NOTICES.
// One draw samples Y + interleaved UV; no RGB intermediate.
struct U {
    rect: vec4<f32>,
    output: vec4<f32>, // width, height, sRGB output, reserved
    range: vec4<f32>, // Y offset/range, C offset/range
    yuv: vec4<f32>, // Kr, Kb, stored-code scale, transfer
    color: vec4<f32>, // BT.2020 primaries, source peak nits, reserved
};
@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var y_plane: texture_2d<f32>;
@group(0) @binding(2) var uv_plane: texture_2d<f32>;

struct Vertex { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex fn vs(@builtin(vertex_index) i: u32) -> Vertex {
    let corners = array<vec2<f32>, 6>(vec2(0.0, 0.0), vec2(1.0, 0.0), vec2(0.0, 1.0), vec2(0.0, 1.0), vec2(1.0, 0.0), vec2(1.0, 1.0));
    let uv = corners[i];
    let p = u.rect.xy + uv * u.rect.zw;
    return Vertex(vec4(p.x / u.output.x * 2.0 - 1.0, 1.0 - p.y / u.output.y * 2.0, 0.0, 1.0), uv);
}

fn srgb_to_linear(v: vec3<f32>) -> vec3<f32> {
    return select(pow((v + 0.055) / 1.055, vec3(2.4)), v / 12.92, v <= vec3(0.04045));
}
fn linear_to_srgb(v: vec3<f32>) -> vec3<f32> {
    return select(1.055 * pow(v, vec3(1.0 / 2.4)) - 0.055, v * 12.92, v <= vec3(0.0031308));
}
// Explicit float interpolation avoids low-precision UNORM sampler interpolation becoming
// visible after HDR/gamut conversion, especially near the BT.2020 gamut boundary.
fn sample_plane(tex: texture_2d<f32>, uv: vec2<f32>) -> vec4<f32> {
    let dim = vec2<i32>(textureDimensions(tex));
    let p = uv * vec2<f32>(dim) - 0.5;
    let i = vec2<i32>(floor(p));
    let f = fract(p);
    let a = textureLoad(tex, clamp(i, vec2(0), dim - 1), 0);
    let b = textureLoad(tex, clamp(i + vec2(1, 0), vec2(0), dim - 1), 0);
    let c = textureLoad(tex, clamp(i + vec2(0, 1), vec2(0), dim - 1), 0);
    let d = textureLoad(tex, clamp(i + vec2(1, 1), vec2(0), dim - 1), 0);
    return mix(mix(a, b, f.x), mix(c, d, f.x), f.y);
}
// SMPTE ST 2084: 1 = 10,000 cd/m².
fn pq(e: f32) -> f32 {
    let p = pow(max(e, 0.0), 1.0 / 78.84375);
    return pow(max(p - 0.8359375, 0.0) / (18.8515625 - 18.6875 * p), 1.0 / 0.1593017578125);
}
fn pq_inv(y: f32) -> f32 {
    let p = pow(clamp(y, 0.0, 1.0), 0.1593017578125);
    return pow((0.8359375 + 18.8515625 * p) / (1.0 + 18.6875 * p), 78.84375);
}
fn hlg_inv(e: vec3<f32>) -> vec3<f32> {
    return select((exp((e - 0.55991073) / 0.17883277) + 0.28466892) / 12.0, e * e / 3.0, e <= vec3(0.5));
}
// ITU-R BT.2390-11 §5.4.1: zero-black EETF, Hermite knee in the PQ domain.
fn eetf(nits: f32, s: f32) -> f32 {
    let d = 203.0;
    if d >= s || nits <= 0.0 { return clamp(nits, 0.0, max(s, d)); }
    let smax = pq_inv(s / 10000.0);
    let e1 = min(pq_inv(nits / 10000.0) / smax, 1.0);
    let max_lum = pq_inv(d / 10000.0) / smax;
    let ks = 1.5 * max_lum - 0.5;
    var e2 = e1;
    if e1 >= ks {
        let t = (e1 - ks) / (1.0 - ks);
        let t2 = t * t; let t3 = t2 * t;
        e2 = (2.0 * t3 - 3.0 * t2 + 1.0) * ks + (t3 - 2.0 * t2 + t) * (1.0 - ks) + (-2.0 * t3 + 3.0 * t2) * max_lum;
    }
    return pq(e2 * smax) * 10000.0;
}
fn display(rgb: vec3<f32>) -> vec3<f32> {
    let v = clamp(rgb, vec3(0.0), vec3(1.0));
    let curve = u32(u.yuv.w);
    var c = pow(v, vec3(2.4)); // BT.1886 zero-black reference display
    if curve == 1u { c = srgb_to_linear(v); }
    if curve == 2u { c = vec3(pq(v.r), pq(v.g), pq(v.b)) * (10000.0 / 203.0); }
    if curve == 3u {
        c = hlg_inv(v);
        var weights = vec3(0.2126, 0.7152, 0.0722);
        if u.color.x > 0.5 { weights = vec3(0.2627, 0.6780, 0.0593); }
        c *= (1000.0 / 203.0) * pow(max(dot(weights, c), 0.0), 0.2);
    }
    if u.color.x > 0.5 {
        // BT.2087, linear BT.2020 → BT.709 (D65).
        c = vec3(dot(vec3(1.660491, -0.587641, -0.072850), c),
                 dot(vec3(-0.124550, 1.132900, -0.008349), c),
                 dot(vec3(-0.018151, -0.100579, 1.118730), c));
    }
    if curve >= 2u {
        let m = max(max(c.r, c.g), c.b);
        if m > 0.0 {
            let nits = m * 203.0;
            if nits >= u.color.y { c /= m; }
            else if nits > 1e-6 { c *= eetf(nits, u.color.y) / nits; }
        }
    }
    // FilmCraft's luminance-preserving desaturation of negative wide-gamut components.
    let mn = min(min(c.r, c.g), c.b);
    if mn < 0.0 {
        let y = dot(vec3(0.2126, 0.7152, 0.0722), c);
        if y <= 0.0 { c = vec3(0.0); }
        else { c = max(y + (c - y) * (y / (y - mn)), vec3(0.0)); }
    }
    c = clamp(c, vec3(0.0), vec3(1.0));
    if u.output.z > 0.5 { c = linear_to_srgb(c); }
    return c;
}
@fragment fn fs(in: Vertex) -> @location(0) vec4<f32> {
    let yc = sample_plane(y_plane, in.uv).r * u.yuv.z;
    let uv = sample_plane(uv_plane, in.uv).rg * u.yuv.z;
    let y = (yc - u.range.x) / u.range.y;
    let cbcr = (uv - u.range.z) / u.range.w;
    let r = y + 2.0 * (1.0 - u.yuv.x) * cbcr.y;
    let b = y + 2.0 * (1.0 - u.yuv.y) * cbcr.x;
    let g = (y - u.yuv.x * r - u.yuv.y * b) / (1.0 - u.yuv.x - u.yuv.y);
    return vec4(display(vec3(r, g, b)), 1.0);
}
