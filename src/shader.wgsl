// Draws one image as a screen-aligned quad.
//
// The quad is positioned by a scale/translate in clip space and the texture is sampled
// through a 2x2 UV matrix that applies EXIF orientation, so no pixels are ever rotated
// on the CPU.
//
// The texture is Rgba8UnormSrgb and the surface is sRGB, so the hardware converts
// sRGB->linear on sample and linear->sRGB on write. That is the whole of this program's
// colour management (REQUIREMENTS.md R10): filtering happens in linear light, which is
// what makes downscaled detail match other viewers instead of coming out dark.

struct Transform {
    // Half-extent of the quad in clip space.
    scale: vec2<f32>,
    // Centre offset in clip space.
    offset: vec2<f32>,
    // Row-major 2x2 applied to (uv - 0.5).
    uv: vec4<f32>,
};

@group(0) @binding(0) var<uniform> xf: Transform;
@group(0) @binding(1) var tex: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs(@builtin(vertex_index) idx: u32) -> VsOut {
    // Two triangles as a strip-like list: unit quad in 0..1, then remapped.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(0.0, 1.0),
    );
    let c = corners[idx];

    var out: VsOut;
    // Clip space: x right, y up. Screen UV has y down, hence the sign flip on y.
    let clip = vec2<f32>(
        (c.x * 2.0 - 1.0) * xf.scale.x + xf.offset.x,
        -((c.y * 2.0 - 1.0) * xf.scale.y) + xf.offset.y,
    );
    out.pos = vec4<f32>(clip, 0.0, 1.0);

    // Apply orientation about the texture centre.
    let d = c - vec2<f32>(0.5, 0.5);
    out.uv = vec2<f32>(
        xf.uv.x * d.x + xf.uv.y * d.y,
        xf.uv.z * d.x + xf.uv.w * d.y,
    ) + vec2<f32>(0.5, 0.5);
    return out;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(tex, samp, in.uv);
}
