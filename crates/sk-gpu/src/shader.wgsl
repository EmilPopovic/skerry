// SPDX-License-Identifier: Apache-2.0
// One pipeline for the whole program. Every quad in the frame — row backgrounds,
// selection, focus rings, glyph coverage and colour glyphs — is an instance
// here, which is what makes the GPU side of this workload trivial and the CPU
// side the thing worth designing.

struct Uniforms {
    viewport: vec2<f32>,
    mask_size: vec2<f32>,
    color_size: vec2<f32>,
    image_size: vec2<f32>,
};

@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var mask_texture: texture_2d<f32>;
@group(0) @binding(2) var color_texture: texture_2d<f32>;
@group(0) @binding(3) var atlas_sampler: sampler;
@group(0) @binding(4) var image_texture: texture_2d<f32>;
// **A second sampler, and it is the only filtering one.** Glyphs are drawn 1:1
// from a raster made at their device size; a thumbnail is 128 texels drawn at
// forty, so it is always minified and nearest sampling would be the aliasing the
// child's box filter exists to avoid.
@group(0) @binding(5) var image_sampler: sampler;
// The one preview, which is not in the atlas because it is not the atlas's size —
// ADR 0018. It shares the filtering sampler above.
@group(0) @binding(6) var preview_texture: texture_2d<f32>;

const KIND_RECT: u32 = 0u;
const KIND_MASK: u32 = 1u;
const KIND_COLOR: u32 = 2u;
// Four kinds, one per direction: up, down, left, right.
const KIND_TRIANGLE: u32 = 3u;
// After the four triangles. A thumbnail from the image atlas — ADR 0018.
const KIND_IMAGE: u32 = 7u;
const KIND_PREVIEW: u32 = 8u;

struct Instance {
    @location(0) rect: vec4<f32>,   // device px: x, y, w, h
    @location(1) uv: vec4<f32>,     // atlas px: x, y, w, h
    @location(2) color: u32,        // packed rgba8, non-premultiplied
    @location(3) kind: u32,
    @location(4) radius: f32,
    @location(5) border: f32,
};

struct Vertex {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) color: vec4<f32>,
    @location(2) @interpolate(flat) kind: u32,
    @location(3) local: vec2<f32>,
    @location(4) @interpolate(flat) half_size: vec2<f32>,
    @location(5) @interpolate(flat) radius: f32,
    @location(6) @interpolate(flat) border: f32,
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

    var atlas = select(u.mask_size, u.color_size, instance.kind == KIND_COLOR);
    if instance.kind == KIND_IMAGE {
        atlas = u.image_size;
    }
    // A preview's texture is exactly its own size, so its uv is its own extent —
    // which the instance already carries, and dividing by it gives 0..1.
    if instance.kind == KIND_PREVIEW {
        atlas = instance.uv.zw;
    }

    var out: Vertex;
    out.position = vec4<f32>(clip, 0.0, 1.0);
    out.uv = (instance.uv.xy + corner * instance.uv.zw) / atlas;
    out.color = unpack4x8unorm(instance.color);
    out.kind = instance.kind;
    out.local = corner * instance.rect.zw;
    out.half_size = instance.rect.zw * 0.5;
    out.radius = instance.radius;
    out.border = instance.border;
    return out;
}

/// Signed distance to an isosceles triangle with its apex at the top of the box,
/// negative inside.
///
/// The maximum of the signed distances to the three edge lines. Exact along an
/// edge and a little short near a vertex, which sharpens a nine-pixel arrowhead
/// rather than blunting it.
fn triangle_up(point: vec2<f32>, half_size: vec2<f32>) -> f32 {
    let w = half_size.x;
    let h = half_size.y;
    let slant = sqrt(4.0 * h * h + w * w);
    let base = point.y - h;
    let left = -(2.0 * h * point.x + w * (point.y + h)) / slant;
    let right = (2.0 * h * point.x - w * (point.y + h)) / slant;
    return max(base, max(left, right));
}

/// The same triangle, turned. The point is rotated rather than the shape, so
/// there is one distance function and four ways into it.
fn triangle(point: vec2<f32>, half_size: vec2<f32>, kind: u32) -> f32 {
    switch kind - KIND_TRIANGLE {
        case 1u: { return triangle_up(vec2<f32>(point.x, -point.y), half_size); }
        case 2u: { return triangle_up(vec2<f32>(point.y, point.x), half_size.yx); }
        case 3u: { return triangle_up(vec2<f32>(point.y, -point.x), half_size.yx); }
        default: { return triangle_up(point, half_size); }
    }
}

/// Signed distance to a rounded box, negative inside.
fn rounded_box(point: vec2<f32>, half_size: vec2<f32>, radius: f32) -> f32 {
    let r = min(radius, min(half_size.x, half_size.y));
    let q = abs(point) - half_size + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

@fragment
fn fs(in: Vertex) -> @location(0) vec4<f32> {
    if in.kind == KIND_RECT {
        // One pixel of analytic antialiasing. A rounded corner drawn without it
        // is a staircase, and a staircase is the difference between a window
        // that looks drawn and one that looks rendered.
        let distance = rounded_box(in.local - in.half_size, in.half_size, in.radius);
        var coverage = clamp(0.5 - distance, 0.0, 1.0);
        if in.border > 0.0 {
            // The ring between the outer edge and `border` pixels inside it.
            coverage = coverage * clamp(distance + in.border + 0.5, 0.0, 1.0);
        }
        let alpha = in.color.a * coverage;
        return vec4<f32>(in.color.rgb * alpha, alpha);
    }
    if in.kind == KIND_PREVIEW {
        let texel = textureSample(preview_texture, image_sampler, in.uv);
        let alpha = texel.a * in.color.a;
        return vec4<f32>(texel.rgb * alpha, alpha);
    }
    if in.kind == KIND_IMAGE {
        // Straight RGBA from the child, premultiplied here — the blend state is
        // premultiplied and a straight buffer drawn through it has dark fringes
        // wherever it is transparent, which is every icon with a rounded corner.
        let texel = textureSample(image_texture, image_sampler, in.uv);
        let alpha = texel.a * in.color.a;
        return vec4<f32>(texel.rgb * alpha, alpha);
    }
    if in.kind >= KIND_TRIANGLE {
        let distance = triangle(in.local - in.half_size, in.half_size, in.kind);
        let alpha = in.color.a * clamp(0.5 - distance, 0.0, 1.0);
        return vec4<f32>(in.color.rgb * alpha, alpha);
    }
    if in.kind == KIND_MASK {
        let coverage = textureSample(mask_texture, atlas_sampler, in.uv).r;
        let alpha = in.color.a * coverage;
        return vec4<f32>(in.color.rgb * alpha, alpha);
    }
    // Colour glyphs arrive from the rasterizer already premultiplied.
    return textureSample(color_texture, atlas_sampler, in.uv);
}
