import package::fullscreen::{VertexOutput, fullscreen_vertex_flipped};

// A pass composites up to this many layers, in order, at each pixel. Every
// layer has a texture binding of its own: with the backdrop that makes nine,
// inside the 16 a WebGL2-class device offers. `COMPOSITE_BATCH_LAYERS` in
// crates/alpha2-render/src/composite.rs is the same number.
const BATCH_LAYERS: u32 = 8u;

// How the intermediate texture stores a value (0 as is, 1 half float, 2 8-bit
// unorm), set when the pipeline is created, so a pass pays only for its own.
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
    // The target pixels the layer can change, as left, top, right, bottom
    // (exclusive). Elsewhere its coverage is zero.
    bounds: vec4f,
};

struct BatchParams {
    // x: blend space (0 linear light), y: the filter sample budget, z: the
    // number of layers, w: 1 when the pass composites over `background`
    // instead of the backdrop texture.
    controls: vec4f,
    output_dimensions: vec4f,
    // What the first layer composites over: the canvas background, as the
    // intermediate texture would store it, or transparent.
    background: vec4f,
    layers: array<LayerParams, BATCH_LAYERS>,
};
@group(0) @binding(10)
var<uniform> batch: BatchParams;

// Both curves map 1 to 1 exactly. `pow` near 1 is not exact on every GPU,
// and white that comes back a fraction short of 1 survives a 16-bit float
// intermediate as a float step below it, which a matte or a dodge then reads
// as not quite white.
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

// The singular branches of color dodge and burn test for 0 and 1. Zero
// survives every conversion exactly, but white comes back from a linear-light
// round trip (the sRGB curve's `pow`, then a 16-bit float intermediate, whose
// step just below 1 is 2^-11) as much as a step short of 1. A component
// within 1/512 of 1 is therefore white here; the specification's own test is
// exact equality.
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
    // Dividing by zero where the source is white is never selected; the
    // max keeps the division finite on every backend.
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

// A gray has l == n == x and nothing to scale toward l. SetLum can leave one
// a rounding error outside 0..1 (WGSL lets `dot` round and fuse as it likes),
// and dividing by its zero spread would give 0/0, a NaN that reaches the
// output even where the blend's weight is zero. A zero spread is left as it is.
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

fn composite(backdrop_encoded: vec4f, source_encoded: vec4f, mode: f32) -> vec4f {
    let linear_light = batch.controls.x < 0.5;
    let source_alpha = source_encoded.a;
    let backdrop_alpha = backdrop_encoded.a;
    let source = straight(source_encoded, linear_light);
    let backdrop = straight(backdrop_encoded, linear_light);
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

// Integrate the rectangular source boundary over a pixel footprint. Unlike
// a UV bounds test this retains fractional coverage, including subpixel layers.
// The separable footprint is an approximation at rotated corners.
fn rectangle_coverage(uv: vec2f, footprint: vec2f) -> f32 {
    let width = max(footprint, vec2f(0.0000001));
    let lower = max(uv - width * 0.5, vec2f(0.0));
    let upper = min(uv + width * 0.5, vec2f(1.0));
    let coverage = clamp((upper - lower) / width, vec2f(0.0), vec2f(1.0));
    return coverage.x * coverage.y;
}

// Layer `index`'s texture: its size, and a sample of it. Each switch is one
// instruction per case, so the composite and filter code around it is
// compiled once for every step; the one-layer entry point, whose index is
// known when it is compiled, folds it to the first texture.
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
    // One bilinear sample for identity/magnification, more only for minification.
    // Bound work by the renderer quality budget (and at most 8 x 8 samples).
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

// Samples layer `index` at this pixel. In the loop, called only for a 2 x 2
// quad that overlaps the layer's bounds, so every lane of a quad takes the
// same branch and the derivatives are those of the whole quad; the
// uniformity analysis cannot see that, so its check is off here.
@diagnostic(off, derivative_uniformity)
fn sample_source(input: VertexOutput, index: u32, looped: bool) -> SampledSource {
    let layer = batch.layers[index];
    let position = layer.position_anchor.xy;
    let anchor = layer.position_anchor.zw;
    let scale = layer.scale_rotation_opacity.xy;
    let angle = layer.scale_rotation_opacity.z;
    let opacity = layer.scale_rotation_opacity.w;
    let output_dimensions = batch.output_dimensions.xy;
    // The pixel's UV from its exact center, not the interpolated `input.uv`:
    // across the full-screen triangle some GPUs (Apple's) interpolate it a
    // quarter texel off at some pixels, which bilinear filtering blends with a
    // neighbour even when the layer maps texels 1:1.
    let offset = input.position.xy / output_dimensions - position;
    // Normalized UV axes have different physical lengths on a non-square
    // target. Rotate in output-pixel space so rotation does not introduce an
    // aspect-dependent non-uniform scale, then return to normalized space
    // before applying the caller's intentional per-axis scale.
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
    // In the loop, only the pixels the layer can change: a batch's pass covers
    // other layers' pixels too. A pass of a large layer is drawn inside them
    // already, or covers a target the layer's coverage is zero outside them.
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

// What a pixel holds is decided by the shader, not by the device's
// conversion to the intermediate texture: the same whether its layers draw
// one to a pass or several to a batch, where a layer reads the one before
// from a register instead of the texture.
//
// A half float is rounded toward zero, as the half-float render targets of
// the devices tested store a value (NVIDIA, RADV, lavapipe), keeping
// subnormals. An 8-bit code is rounded to nearest; devices do not round a
// value to the nearest code exactly (NVIDIA and RADV store v * 255 = n.51
// as n), so the pass writes one that is already a code.

// A half float exactly as the texture holds it, for a value of 0 or more
// (every composite result is). From 2^-14 up a half float keeps 10 of the 23
// fraction bits: the others are cut. Below, a subnormal half float has a
// fixed step of 2^-24, which scaling by 2^24 (exact, a power of two) makes a
// whole number to truncate; under 2^-24 nothing is left. Both are computed and
// one selected, without a branch, so a software rasterizer runs one path.
fn stored_half(value: vec4f) -> vec4f {
    let normal = bitcast<vec4f>(bitcast<vec4u>(value) & vec4u(0xffffe000u));
    let subnormal = trunc(value * 16777216.0) * (1.0 / 16777216.0);
    return select(subnormal, normal, value >= vec4f(1.0 / 16384.0));
}

fn stored_unorm8(value: vec4f) -> vec4f {
    return round(clamp(value, vec4f(0.0), vec4f(1.0)) * 255.0) / 255.0;
}

// `value` exactly as the intermediate texture holds it: what the next layer
// of a batch reads.
fn stored(value: vec4f) -> vec4f {
    if STORED_FORMAT == 1u {
        return stored_half(value);
    }
    if STORED_FORMAT == 2u {
        return stored_unorm8(value);
    }
    return value;
}

// The backdrop as `stored` gives it. A half float reads back exactly; an
// 8-bit code reads back as the device converts it, which may differ from
// `stored`'s quotient in the last bit.
fn stored_backdrop(value: vec4f) -> vec4f {
    if STORED_FORMAT == 2u {
        return stored_unorm8(value);
    }
    return value;
}

// `value` for the one-layer entry point to write. A half float is cut to its
// 10 fraction bits, which costs one operation where `stored` costs several on
// every pixel of a full-frame pass; only a subnormal is left to the device,
// whose rounding toward zero agrees.
fn stored_pass(value: vec4f) -> vec4f {
    if STORED_FORMAT == 1u {
        return bitcast<vec4f>(bitcast<vec4u>(value) & vec4u(0xffffe000u));
    }
    if STORED_FORMAT == 2u {
        return stored_unorm8(value);
    }
    return value;
}

// Composites layer `index` of the pass over `backdrop`: the one composite
// both entry points run, so a blend mode or an effect changes in one place.
//
// `looped` (a constant at each call) is true in the loop of `fs`, which
// composites every small layer, alone or in a batch: a layer's step runs only
// on the 2 x 2 quads (which derivatives difference across) that overlap its
// bounds, and its result is exactly what the intermediate texture holds, as
// the next layer of a batch reads it from a register. It is false in
// `fs_layer`, which composites a large layer alone.
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

// What the pass composites over: the canvas background in the first pass,
// so no pass clears the target first, and what the backdrop texture holds at
// the pixel in the others.
fn pass_backdrop(input: VertexOutput) -> vec4f {
    var backdrop = batch.background;
    if batch.controls.w < 0.5 {
        backdrop = textureLoad(backdrop_texture, vec2i(input.position.xy), 0);
    }
    return stored_backdrop(backdrop);
}

// Small layers (under half the target): the pass's layers
// (`batch.controls.z`, one to `BATCH_LAYERS`) over the backdrop in order,
// each result as the intermediate texture holds it. A small layer drawn alone
// and a batch of eight run this same entry point in the same pipeline, the
// count a uniform, so no compiler can optimize a pass of one apart from a
// batch and store a different last bit.
@fragment
fn fs(input: VertexOutput) -> @location(0) vec4f {
    var color = pass_backdrop(input);
    let count = min(u32(batch.controls.z + 0.5), BATCH_LAYERS);
    for (var index = 0u; index < count; index++) {
        color = composite_layer(input, color, index, true);
    }
    return color;
}

// A large layer (half the target or more), which never shares a pass: one
// step, without the loop's bounds tests. No batch result is compared with
// it, and a device that runs every step of a shader whether it is taken or
// not (a software rasterizer, which composites every layer here) pays for
// one.
@fragment
fn fs_layer(input: VertexOutput) -> @location(0) vec4f {
    return composite_layer(input, pass_backdrop(input), 0u, false);
}

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex_flipped(index);
}
