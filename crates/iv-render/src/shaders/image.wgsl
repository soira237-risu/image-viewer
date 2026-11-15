// Draws the photo as one quad. Orientation is a UV transform; transparency composites over a
// checkerboard. The texture is sRGB, so filtering and blending happen in linear light.

struct Params {
    // destination rectangle in screen pixels: x, y, width, height
    dest: vec4<f32>,
    // viewport width, height, checker cell size in pixels, flags (bit0 alpha, bit1 nearest)
    viewport: vec4<f32>,
    // displayed UV -> stored UV, rows of a 2x3 matrix
    uv_row0: vec4<f32>,
    uv_row1: vec4<f32>,
    // checkerboard colours (linear)
    checker_a: vec4<f32>,
    checker_b: vec4<f32>,
    // selection in screen pixels: x, y, width, height (width 0 = none); outside is dimmed
    selection: vec4<f32>,
};

@group(0) @binding(0) var<uniform> p: Params;
@group(0) @binding(1) var photo: texture_2d<f32>;
@group(0) @binding(2) var smp_linear: sampler;
@group(0) @binding(3) var smp_nearest: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    // triangle strip: (0,0) (1,0) (0,1) (1,1)
    let corner = vec2<f32>(f32(i & 1u), f32(i >> 1u));
    let px = p.dest.xy + corner * p.dest.zw;
    let ndc = vec2<f32>(px.x / p.viewport.x * 2.0 - 1.0, 1.0 - px.y / p.viewport.y * 2.0);
    var out: VsOut;
    out.pos = vec4<f32>(ndc, 0.0, 1.0);
    out.uv = corner;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let d = vec3<f32>(in.uv, 1.0);
    let st = vec2<f32>(dot(p.uv_row0.xyz, d), dot(p.uv_row1.xyz, d));
    let flags = u32(p.viewport.w);

    var c: vec4<f32>;
    if ((flags & 2u) != 0u) {
        c = textureSample(photo, smp_nearest, st);
    } else {
        c = textureSample(photo, smp_linear, st);
    }

    var rgb = c.rgb;
    if ((flags & 1u) != 0u) {
        let cell = floor(in.pos.xy / p.viewport.z);
        let odd = (u32(cell.x) + u32(cell.y)) & 1u;
        let bg = select(p.checker_a.rgb, p.checker_b.rgb, odd == 1u);
        rgb = mix(bg, c.rgb, c.a);
    }
    if (p.selection.z > 0.0) {
        let lo = p.selection.xy;
        let hi = p.selection.xy + p.selection.zw;
        let inside = all(in.pos.xy >= lo) && all(in.pos.xy < hi);
        if (!inside) {
            rgb = rgb * 0.35;
        }
    }
    return vec4<f32>(rgb, 1.0);
}
