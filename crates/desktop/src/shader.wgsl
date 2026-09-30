// Single-pass UI shader.
//
// Every primitive is one instanced quad (triangle strip, 4 vertices).
// `params.w` selects what the quad is:
//   0 = rounded rectangle, optionally bordered (border colour in `color2`)
//       or blurred into a soft shadow (`params.z` > 0)
//   1 = glyph, alpha sampled from the R8 atlas
//   2 = image, RGBA sampled from the image atlas, clipped to a rounded
//       rectangle (radius `params.x`) and faded by `color.a`
//
// Geometry arrives in logical pixels and is scaled to physical pixels here.
// Colours are straight-alpha sRGB; output is premultiplied.

struct Globals {
    viewport: vec2<f32>,   // physical pixels
    scale: f32,            // physical pixels per logical pixel
    linear_output: f32,    // 1.0 when the surface format is sRGB-encoded
    atlas_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var atlas_sampler: sampler;
@group(0) @binding(3) var images: texture_2d<f32>;

struct Instance {
    @location(0) rect: vec4<f32>,    // x, y, w, h
    @location(1) clip: vec4<f32>,    // x0, y0, x1, y1
    @location(2) color: vec4<f32>,
    @location(3) color2: vec4<f32>,
    @location(4) uv: vec4<f32>,      // atlas texels: x, y, w, h
    @location(5) params: vec4<f32>,  // radius, border width, blur, kind
};

struct VertexOut {
    @builtin(position) position: vec4<f32>,
    @location(0) local: vec2<f32>,
    @location(1) uv: vec2<f32>,
    @location(2) @interpolate(flat) half_size: vec2<f32>,
    @location(3) @interpolate(flat) color: vec4<f32>,
    @location(4) @interpolate(flat) color2: vec4<f32>,
    @location(5) @interpolate(flat) params: vec4<f32>,
    @location(6) @interpolate(flat) clip: vec4<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vertex: u32, inst: Instance) -> VertexOut {
    let corner = vec2<f32>(f32(vertex & 1u), f32(vertex >> 1u));
    let s = globals.scale;
    let kind = inst.params.w;

    var origin = inst.rect.xy * s;
    var size = inst.rect.zw * s;
    var pad = 0.0;
    if kind != 1.0 {
        // Snap shapes to the pixel grid so 1px borders stay crisp, and grow
        // the quad to leave room for anti-aliasing and blur falloff.
        origin = round(origin);
        size = round(size);
        pad = inst.params.z * s + 1.0;
    }
    let p = origin - pad + corner * (size + 2.0 * pad);

    var out: VertexOut;
    out.position = vec4<f32>(p / globals.viewport * vec2<f32>(2.0, -2.0) + vec2<f32>(-1.0, 1.0), 0.0, 1.0);
    out.local = p - (origin + size * 0.5);
    // Relative to the unpadded quad, so padding samples just outside it.
    let rel = (p - origin) / max(size, vec2<f32>(1e-3));
    out.uv = (inst.uv.xy + rel * inst.uv.zw) / globals.atlas_size;
    out.half_size = size * 0.5;
    out.color = inst.color;
    out.color2 = inst.color2;
    out.params = vec4<f32>(inst.params.xyz * s, kind);
    out.clip = inst.clip * s;
    return out;
}

// Signed distance from `p` to a rounded box centred on the origin.
fn rounded_box(p: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_size.x, half_size.y));
    let q = abs(p) - half_size + r;
    return length(max(q, vec2<f32>(0.0))) + min(max(q.x, q.y), 0.0) - r;
}

fn srgb_to_linear(c: vec3<f32>) -> vec3<f32> {
    let low = c / 12.92;
    let high = pow((c + 0.055) / 1.055, vec3<f32>(2.4));
    return select(high, low, c <= vec3<f32>(0.04045));
}

@fragment
fn fs_main(in: VertexOut) -> @location(0) vec4<f32> {
    let pos = in.position.xy;
    if pos.x < in.clip.x || pos.y < in.clip.y || pos.x > in.clip.z || pos.y > in.clip.w {
        discard;
    }

    let kind = in.params.w;
    var color: vec4<f32>;
    if kind == 1.0 {
        let coverage = textureSampleLevel(atlas, atlas_sampler, in.uv, 0.0).r;
        color = vec4<f32>(in.color.rgb, in.color.a * coverage);
    } else if kind == 2.0 {
        let texel = textureSampleLevel(images, atlas_sampler, in.uv, 0.0);
        let d = rounded_box(in.local, in.half_size, in.params.x);
        color = vec4<f32>(texel.rgb, texel.a * in.color.a * clamp(0.5 - d, 0.0, 1.0));
    } else {
        let d = rounded_box(in.local, in.half_size, in.params.x);
        let blur = in.params.z;
        if blur > 0.0 {
            color = vec4<f32>(in.color.rgb, in.color.a * (1.0 - smoothstep(-blur, blur, d)));
        } else {
            var fill = in.color;
            if in.params.y > 0.0 {
                let inside = clamp(0.5 - (d + in.params.y), 0.0, 1.0);
                fill = mix(in.color2, fill, inside);
            }
            color = vec4<f32>(fill.rgb, fill.a * clamp(0.5 - d, 0.0, 1.0));
        }
    }

    if globals.linear_output > 0.5 {
        color = vec4<f32>(srgb_to_linear(color.rgb), color.a);
    }
    return vec4<f32>(color.rgb * color.a, color.a);
}
