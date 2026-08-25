// One pipeline, one draw call. Every quad in the frame — row backgrounds,
// selection, icons, glyph masks and colour glyphs — is an instance here, which
// is what makes the GPU side of this workload trivial and the CPU side the
// thing actually under test.

struct Uniforms {
    viewport: vec2<f32>,
    mask_size: vec2<f32>,
    color_size: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var mask_tex: texture_2d<f32>;
@group(0) @binding(2) var color_tex: texture_2d<f32>;
@group(0) @binding(3) var atlas_sampler: sampler;

const KIND_SOLID: u32 = 0u;
const KIND_MASK: u32 = 1u;
const KIND_COLOR: u32 = 2u;

struct Instance {
    @location(0) rect: vec4<f32>,   // device px: x, y, w, h
    @location(1) uv: vec4<f32>,     // atlas px: x, y, w, h
    @location(2) color: u32,        // packed rgba8
    @location(3) kind: u32,
};

struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
};

@vertex
fn vs(@builtin(vertex_index) index: u32, instance: Instance) -> Vertex {
    // Two triangles from six vertex indices; no vertex buffer, no index buffer.
    let corner = vec2<f32>(
        f32((index == 1u) || (index == 2u) || (index == 4u)),
        f32((index == 2u) || (index == 4u) || (index == 5u)),
    );

    let device = instance.rect.xy + corner * instance.rect.zw;
    // Device pixels to clip space, y down.
    let clip = vec2<f32>(
        device.x / u.viewport.x * 2.0 - 1.0,
        1.0 - device.y / u.viewport.y * 2.0,
    );

    let atlas = select(u.mask_size, u.color_size, instance.kind == KIND_COLOR);

    var out: Vertex;
    out.position = vec4<f32>(clip, 0.0, 1.0);
    out.uv = (instance.uv.xy + corner * instance.uv.zw) / atlas;
    out.color = unpack4x8unorm(instance.color);
    out.kind = instance.kind;
    return out;
}

@fragment
fn fs(in: Vertex) -> @location(0) vec4<f32> {
    if in.kind == KIND_SOLID {
        return vec4<f32>(in.color.rgb * in.color.a, in.color.a);
    }
    if in.kind == KIND_MASK {
        let coverage = textureSample(mask_tex, atlas_sampler, in.uv).r;
        let alpha = in.color.a * coverage;
        return vec4<f32>(in.color.rgb * alpha, alpha);
    }
    // Colour glyphs arrive from swash already premultiplied.
    return textureSample(color_tex, atlas_sampler, in.uv);
}
