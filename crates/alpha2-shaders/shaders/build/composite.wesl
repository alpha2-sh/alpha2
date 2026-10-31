import package::fullscreen::{VertexOutput, fullscreen_vertex_flipped};

// Layers per pass, each with its own texture binding; with the backdrop that is nine, within
// WebGL2's 16. Must match `COMPOSITE_BATCH_LAYERS` in crates/alpha2-render/src/composite.rs.
const BATCH_LAYERS: u32 = 8u;

// Intermediate storage format (0 as is, 1 half float, 2 8-bit unorm), fixed per pipeline.
override STORED_FORMAT: u32 = 1u;

@group(0) @binding(0)
var source_0: texture_2d<f32>;
@group(0) @binding(1)
var source_1: texture_2d<f32>;
@group(0) @binding(2)
var source_2: texture_2d<f32>;
@group(0) @binding(3)
var source_3: texture_2d<f32>;
@group(0) @binding(4)
var source_4: texture_2d<f32>;
@group(0) @binding(5)
var source_5: texture_2d<f32>;
@group(0) @binding(6)
var source_6: texture_2d<f32>;
@group(0) @binding(7)
var source_7: texture_2d<f32>;
@group(0) @binding(8)
var source_sampler: sampler;
@group(0) @binding(9)
var backdrop_texture: texture_2d<f32>;

struct LayerParams {
    position_anchor: vec4f,
    scale_rotation_opacity: vec4f,
    // x: blend mode.
    blend: vec4f,
    // Target pixels the layer can change: left, top, right, bottom (exclusive).
    bounds: vec4f,
};

struct BatchParams {
    // x: blend space (0 linear light), y: filter sample budget, z: layer count, w: 1 to
    // composite over `background` instead of the backdrop texture.
    controls: vec4f,
    output_dimensions: vec4f,
    // What the first layer composites over: the stored canvas background, or transparent.
    background: vec4f,
    layers: array<LayerParams, BATCH_LAYERS>,
};
@group(0) @binding(10)
var<uniform> batch: BatchParams;

// Both curves map 1 to 1 exactly. `pow` near 1 is inexact on some GPUs, and white a step short
// of 1 in a half-float intermediate would read as not quite white in mattes and dodge.
fn srgb_to_linear_component(value: f32) -> f32 {
    if value <= 0.04045 { return value / 12.92; }
    if value >= 1.0 { return 1.0; }
    return pow((value + 0.055) / 1.055, 2.4);
}

fn linear_to_srgb_component(value: f32) -> f32 {
    if value <= 0.0031308 { return value * 12.92; }
    if value >= 1.0 { return 1.0; }
    return 1.055 * pow(value, 1.0 / 2.4) - 0.055;
}

fn srgb_to_linear(color: vec3f) -> vec3f {
    return vec3f(
        srgb_to_linear_component(color.r),
        srgb_to_linear_component(color.g),
        srgb_to_linear_component(color.b),
    );
}

fn linear_to_srgb(color: vec3f) -> vec3f {
    return vec3f(
        linear_to_srgb_component(color.r),
        linear_to_srgb_component(color.g),
        linear_to_srgb_component(color.b),
    );
}

fn straight(encoded: vec4f, linear_light: bool) -> vec3f {
    if encoded.a <= 0.000001 { return vec3f(0.0); }
    let color = clamp(encoded.rgb / encoded.a, vec3f(0.0), vec3f(1.0));
    if linear_light { return srgb_to_linear(color); }
    return color;
}

// W3C Compositing and Blending Level 1, section 9: B(Cb, Cs) on straight
// colors. `crates/alpha2-render/src/tests/blend_reference.rs` is the CPU
// reference every mode is tested against.

// Color dodge and burn branch on 0 and 1. White can come back from a linear-light round trip
// up to one half-float step (2^-11) short of 1, so components within 1/512 of 1 count as white
// (the specification tests exact equality).
const BLEND_WHITE: f32 = 1.0 - 1.0 / 512.0;

fn blend_screen(backdrop: vec3f, source: vec3f) -> vec3f {
    return backdrop + source - backdrop * source;
}

fn blend_hard_light(backdrop: vec3f, source: vec3f) -> vec3f {
    return select(
        blend_screen(backdrop, 2.0 * source - 1.0),
        backdrop * (2.0 * source),
        source <= vec3f(0.5),
    );
}

fn blend_color_dodge(backdrop: vec3f, source: vec3f) -> vec3f {
    // The white-source branch is never selected; max keeps the division finite on every backend.
    let dodged = min(vec3f(1.0), backdrop / max(1.0 - source, vec3f(0.000001)));
    return select(
        select(dodged, vec3f(1.0), source >= vec3f(BLEND_WHITE)),
        vec3f(0.0),
        backdrop <= vec3f(0.0),
    );
}

fn blend_color_burn(backdrop: vec3f, source: vec3f) -> vec3f {
    let burned = 1.0 - min(vec3f(1.0), (1.0 - backdrop) / max(source, vec3f(0.000001)));
    return select(
        select(burned, vec3f(0.0), source <= vec3f(0.0)),
        vec3f(1.0),
        backdrop >= vec3f(BLEND_WHITE),
    );
}

fn blend_soft_light(backdrop: vec3f, source: vec3f) -> vec3f {
    let d = select(
        sqrt(backdrop),
        ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop,
        backdrop <= vec3f(0.25),
    );
    return select(
        backdrop + (2.0 * source - 1.0) * (d - backdrop),
        backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop),
        source <= vec3f(0.5),
    );
}

fn blend_lum(color: vec3f) -> f32 {
    return dot(color, vec3f(0.3, 0.59, 0.11));
}

// SetLum can leave a gray a rounding error outside 0..1, and dividing by its zero spread would
// give a NaN that reaches the output even at zero blend weight, so a zero spread is left as is.
fn blend_clip_color(color: vec3f) -> vec3f {
    let l = blend_lum(color);
    let n = min(color.r, min(color.g, color.b));
    let x = max(color.r, max(color.g, color.b));
    var clipped = color;
    if n < 0.0 && l - n > 0.0 {
        clipped = l + (clipped - l) * l / (l - n);
    }
    if x > 1.0 && x - l > 0.0 {
        clipped = l + (clipped - l) * (1.0 - l) / (x - l);
    }
    return clipped;
}

fn blend_set_lum(color: vec3f, l: f32) -> vec3f {
    return blend_clip_color(color + (l - blend_lum(color)));
}

fn blend_sat(color: vec3f) -> f32 {
    return max(color.r, max(color.g, color.b)) - min(color.r, min(color.g, color.b));
}

// SetSat: the largest component becomes `s`, the smallest zero, and the
// middle one keeps its relative position between them.
fn blend_set_sat(color: vec3f, s: f32) -> vec3f {
    let high = max(color.r, max(color.g, color.b));
    let low = min(color.r, min(color.g, color.b));
    if high > low {
        return (color - low) * s / (high - low);
    }
    return vec3f(0.0);
}

fn blend_rgb(backdrop: vec3f, source: vec3f, mode: f32) -> vec3f {
    switch u32(mode + 0.5) {
        case 1u: { return backdrop * source; }
        case 2u: { return blend_screen(backdrop, source); }
        case 3u: { return blend_hard_light(source, backdrop); }
        case 4u: { return min(backdrop, source); }
        case 5u: { return max(backdrop, source); }
        case 6u: { return blend_color_dodge(backdrop, source); }
        case 7u: { return blend_color_burn(backdrop, source); }
        case 8u: { return blend_hard_light(backdrop, source); }
        case 9u: { return blend_soft_light(backdrop, source); }
        case 10u: { return abs(backdrop - source); }
        case 11u: { return backdrop + source - 2.0 * backdrop * source; }
        case 12u: {
            return blend_set_lum(blend_set_sat(source, blend_sat(backdrop)), blend_lum(backdrop));
        }
        case 13u: {
            return blend_set_lum(blend_set_sat(backdrop, blend_sat(source)), blend_lum(backdrop));
        }
        case 14u: { return blend_set_lum(source, blend_lum(backdrop)); }
        case 15u: { return blend_set_lum(backdrop, blend_lum(source)); }
        default: { return source; }
    }
}

// Compositing Level 2 `plus-lighter`: premultiplied color and alpha added and clamped to 1.
fn plus_lighter(
    backdrop: vec3f,
    backdrop_alpha: f32,
    source: vec3f,
    source_alpha: f32,
    linear_light: bool,
) -> vec4f {
    let output_alpha = min(1.0, source_alpha + backdrop_alpha);
    if output_alpha <= 0.000001 { return vec4f(0.0); }
    let premultiplied = min(vec3f(1.0), source * source_alpha + backdrop * backdrop_alpha);
    var output_color = clamp(premultiplied / output_alpha, vec3f(0.0), vec3f(1.0));
    if linear_light { output_color = linear_to_srgb(output_color); }
    return vec4f(output_color * output_alpha, output_alpha);
}

fn composite(backdrop_encoded: vec4f, source_encoded: vec4f, mode: f32) -> vec4f {
    let linear_light = batch.controls.x < 0.5;
    let source_alpha = source_encoded.a;
    let backdrop_alpha = backdrop_encoded.a;
    let source = straight(source_encoded, linear_light);
    let backdrop = straight(backdrop_encoded, linear_light);
    if mode > 15.5 {
        return plus_lighter(backdrop, backdrop_alpha, source, source_alpha, linear_light);
    }
    let blended = blend_rgb(backdrop, source, mode);
    let output_alpha = source_alpha + backdrop_alpha * (1.0 - source_alpha);
    let premultiplied = source_alpha * (1.0 - backdrop_alpha) * source
        + backdrop_alpha * (1.0 - source_alpha) * backdrop
        + source_alpha * backdrop_alpha * blended;
    if output_alpha <= 0.000001 { return vec4f(0.0); }
    var output_color = clamp(premultiplied / output_alpha, vec3f(0.0), vec3f(1.0));
    if linear_light { output_color = linear_to_srgb(output_color); }
    return vec4f(output_color * output_alpha, output_alpha);
}

// Integrates the rectangular source boundary over a pixel footprint, keeping fractional
// coverage. The separable footprint is approximate at rotated corners.
fn rectangle_coverage(uv: vec2f, footprint: vec2f) -> f32 {
    let width = max(footprint, vec2f(0.0000001));
    let lower = max(uv - width * 0.5, vec2f(0.0));
    let upper = min(uv + width * 0.5, vec2f(1.0));
    let coverage = clamp((upper - lower) / width, vec2f(0.0), vec2f(1.0));
    return coverage.x * coverage.y;
}

// Layer `index`'s texture size and samples. The switch keeps the surrounding code compiled once
// for every layer; the one-layer entry point folds it to the first texture.
fn layer_dimensions(index: u32) -> vec2u {
    switch index {
        case 0u: { return textureDimensions(source_0); }
        case 1u: { return textureDimensions(source_1); }
        case 2u: { return textureDimensions(source_2); }
        case 3u: { return textureDimensions(source_3); }
        case 4u: { return textureDimensions(source_4); }
        case 5u: { return textureDimensions(source_5); }
        case 6u: { return textureDimensions(source_6); }
        default: { return textureDimensions(source_7); }
    }
}

fn layer_sample(index: u32, point: vec2f) -> vec4f {
    switch index {
        case 0u: { return textureSampleLevel(source_0, source_sampler, point, 0.0); }
        case 1u: { return textureSampleLevel(source_1, source_sampler, point, 0.0); }
        case 2u: { return textureSampleLevel(source_2, source_sampler, point, 0.0); }
        case 3u: { return textureSampleLevel(source_3, source_sampler, point, 0.0); }
        case 4u: { return textureSampleLevel(source_4, source_sampler, point, 0.0); }
        case 5u: { return textureSampleLevel(source_5, source_sampler, point, 0.0); }
        case 6u: { return textureSampleLevel(source_6, source_sampler, point, 0.0); }
        default: { return textureSampleLevel(source_7, source_sampler, point, 0.0); }
    }
}

fn filtered_source(index: u32, uv: vec2f, dx: vec2f, dy: vec2f) -> vec4f {
    let dimensions = vec2f(layer_dimensions(index));
    // One bilinear sample unless minifying, then up to 8 x 8 within the quality budget.
    let wanted = clamp(ceil(vec2f(length(dx * dimensions), length(dy * dimensions)) - 0.0001), vec2f(1.0), vec2f(8.0));
    let budget = clamp(batch.controls.y, 1.0, 64.0);
    let nx = min(wanted.x, budget);
    let ny = min(wanted.y, max(1.0, floor(budget / nx)));
    let footprint = abs(dx) / nx + abs(dy) / ny;
    var color = vec4f(0.0);
    for (var y = 0u; y < u32(ny); y++) {
        for (var x = 0u; x < u32(nx); x++) {
            let offset = (vec2f(f32(x), f32(y)) + 0.5) / vec2f(nx, ny) - 0.5;
            let point = uv + dx * offset.x + dy * offset.y;
            let coverage = rectangle_coverage(point, footprint);
            // Coverage multiplies associated RGB and alpha together: no dark halo.
            color += layer_sample(index, point) * coverage;
        }
    }
    return color / (nx * ny);
}

struct SampledSource {
    color: vec4f,
    coverage: f32,
};

// Samples layer `index` at this pixel. In the loop it runs only for whole 2 x 2 quads, so
// derivatives are valid even though uniformity analysis cannot prove it.
@diagnostic(off, derivative_uniformity)
fn sample_source(input: VertexOutput, index: u32, looped: bool) -> SampledSource {
    let layer = batch.layers[index];
    let position = layer.position_anchor.xy;
    let anchor = layer.position_anchor.zw;
    let scale = layer.scale_rotation_opacity.xy;
    let angle = layer.scale_rotation_opacity.z;
    let opacity = layer.scale_rotation_opacity.w;
    let output_dimensions = batch.output_dimensions.xy;
    // UV from the exact pixel center rather than `input.uv`, which some GPUs interpolate a
    // quarter texel off, blurring 1:1 layers.
    let offset = input.position.xy / output_dimensions - position;
    // Rotate in output-pixel space so a non-square target does not skew the rotation, then
    // return to normalized space for the per-axis scale.
    let pixel_offset = offset * output_dimensions;
    let cosine = cos(angle);
    let sine = sin(angle);
    let rotated_pixel_offset = vec2f(
        cosine * pixel_offset.x + sine * pixel_offset.y,
        -sine * pixel_offset.x + cosine * pixel_offset.y,
    );
    let local = rotated_pixel_offset / output_dimensions;
    let source_uv = local / scale + anchor;
    // Evaluate derivatives before any per-fragment early return.
    let dx = dpdx(source_uv);
    let dy = dpdy(source_uv);
    var coverage = rectangle_coverage(source_uv, abs(dx) + abs(dy));
    // In the loop, skip pixels outside the layer's bounds, since a batch covers other layers too.
    if looped {
        let pixel = input.position.xy;
        if any(pixel < layer.bounds.xy) || any(pixel >= layer.bounds.zw) {
            coverage = 0.0;
        }
    }
    if coverage <= 0.0 {
        return SampledSource(vec4f(0.0), coverage);
    }
    return SampledSource(filtered_source(index, source_uv, dx, dy) * opacity, coverage);
}

// The shader, not the device's conversion, decides what a pixel stores, so batched layers
// (read from a register) match layers drawn one per pass.
//
// Half floats round toward zero, as half-float render targets store finite values, keeping
// subnormals and infinity. 8-bit values are rounded to the nearest code in the shader because
// some devices truncate (storing v * 255 = n.51 as n).

// A non-negative value as a half-float texture stores it. Normals keep 10 fraction bits;
// subnormals truncate to multiples of 2^-24 via an exact 2^24 scale. Both are computed and one
// selected without branching, so software rasterizers run one path.
fn stored_half(value: vec4f) -> vec4f {
    let normal = bitcast<vec4f>(bitcast<vec4u>(value) & vec4u(0xffffe000u));
    let subnormal = trunc(value * 16777216.0) * (1.0 / 16777216.0);
    return select(subnormal, normal, value >= vec4f(1.0 / 16384.0));
}

fn stored_unorm8(value: vec4f) -> vec4f {
    return round(clamp(value, vec4f(0.0), vec4f(1.0)) * 255.0) / 255.0;
}

// `value` exactly as the intermediate texture holds it, for the next layer of a batch.
fn stored(value: vec4f) -> vec4f {
    if STORED_FORMAT == 1u {
        return stored_half(value);
    }
    if STORED_FORMAT == 2u {
        return stored_unorm8(value);
    }
    return value;
}

// The backdrop as `stored` gives it; 8-bit codes may differ from `stored`'s quotient in the last
// bit, as the device converts them.
fn stored_backdrop(value: vec4f) -> vec4f {
    if STORED_FORMAT == 2u {
        return stored_unorm8(value);
    }
    return value;
}

// `value` for the one-layer entry point: a cheap cut to 10 fraction bits, leaving subnormals to
// the device, whose rounding toward zero agrees.
fn stored_pass(value: vec4f) -> vec4f {
    if STORED_FORMAT == 1u {
        return bitcast<vec4f>(bitcast<vec4u>(value) & vec4u(0xffffe000u));
    }
    if STORED_FORMAT == 2u {
        return stored_unorm8(value);
    }
    return value;
}

// Composites layer `index` over `backdrop`; shared by both entry points.
//
// `looped` is true in `fs`, where a layer runs only on 2 x 2 quads overlapping its bounds and
// its result is rounded as stored. It is false in `fs_layer`, for a large layer alone.
fn composite_layer(
    input: VertexOutput,
    backdrop: vec4f,
    index: u32,
    looped: bool,
) -> vec4f {
    if looped {
        let quad = floor(input.position.xy * 0.5) * 2.0;
        let bounds = batch.layers[index].bounds;
        if any(quad + 2.0 <= bounds.xy) || any(quad >= bounds.zw) {
            return backdrop;
        }
    }
    let source = sample_source(input, index, looped);
    if source.coverage <= 0.0 {
        return backdrop;
    }
    let color = composite(backdrop, source.color, batch.layers[index].blend.x);
    if looped {
        return stored(color);
    }
    return stored_pass(color);
}

// The canvas background in the first pass (so no clear is needed), else the backdrop texture.
fn pass_backdrop(input: VertexOutput) -> vec4f {
    var backdrop = batch.background;
    if batch.controls.w < 0.5 {
        backdrop = textureLoad(backdrop_texture, vec2i(input.position.xy), 0);
    }
    return stored_backdrop(backdrop);
}

// Small layers (under half the target), one to `BATCH_LAYERS` per pass. The count is a uniform
// so single layers and batches run identical code and store identical bits.
@fragment
fn fs(input: VertexOutput) -> @location(0) vec4f {
    var color = pass_backdrop(input);
    let count = min(u32(batch.controls.z + 0.5), BATCH_LAYERS);
    for (var index = 0u; index < count; index++) {
        color = composite_layer(input, color, index, true);
    }
    return color;
}

// A large layer (half the target or more) alone, without the loop's bounds tests, so software
// rasterizers that execute every branch pay for one step.
@fragment
fn fs_layer(input: VertexOutput) -> @location(0) vec4f {
    return composite_layer(input, pass_backdrop(input), 0u, false);
}

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex_flipped(index);
}
