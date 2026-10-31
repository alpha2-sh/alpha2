import package::fullscreen::{VertexOutput, fullscreen_vertex_flipped};

struct PresentationParams {
    output_source_size: vec4f,
    visible_rect: vec4f,
    draw_rect: vec4f,
    // Clockwise quarter turns in bits 0-1; bit 2 mirrors left to right before turning.
    orientation: u32,
    // 0: straight sRGB; 1: premultiplied sRGB; 2: premultiplied linear, stored with the sRGB
    // curve extended beyond 0..1.
    mode: u32,
    // Samples per output pixel allowed for minification, as in the composite.
    max_filter_samples: f32,
    _padding: u32,
};

@group(0) @binding(0)
var source_texture: texture_2d<f32>;

@group(0) @binding(1)
var source_sampler: sampler;

@group(0) @binding(2)
var<uniform> params: PresentationParams;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex_flipped(index);
}

@fragment
fn fs(@location(0) uv: vec2f) -> @location(0) vec4f {
    let output_size = params.output_source_size.xy;
    let destination = uv * output_size;
    let draw_origin = params.draw_rect.xy;
    let draw_size = params.draw_rect.zw;
    let inside = all(destination >= draw_origin)
        && all(destination < draw_origin + draw_size);
    if !inside {
        return vec4f(0.0);
    }
    // Beyond 2x minification, average up to 8 x 8 stored samples within the quality budget, as
    // the composite does, to avoid moire. Up to 2x one bilinear sample suffices.
    let grid = filter_grid();
    if all(grid == vec2f(1.0)) {
        return stored_sample(source_pixel_at(destination));
    }
    var sum = vec4f(0.0);
    for (var y = 0u; y < u32(grid.y); y++) {
        for (var x = 0u; x < u32(grid.x); x++) {
            let offset = (vec2f(f32(x), f32(y)) + 0.5) / grid - 0.5;
            sum += stored_sample(source_pixel_at(destination + offset));
        }
    }
    return sum / (grid.x * grid.y);
}

// Source pixels per output pixel along each output axis, accounting for quarter turns.
fn filter_grid() -> vec2f {
    var extent = params.visible_rect.zw;
    if (params.orientation & 1u) != 0u {
        extent = extent.yx;
    }
    // One bilinear sample covers 2x, so the grid starts beyond that.
    let footprint = extent / params.draw_rect.zw;
    let wanted = select(
        vec2f(1.0),
        clamp(ceil(footprint - 0.0001), vec2f(1.0), vec2f(8.0)),
        footprint > vec2f(2.0),
    );
    let budget = clamp(params.max_filter_samples, 1.0, 64.0);
    let columns = min(wanted.x, budget);
    let rows = min(wanted.y, max(1.0, floor(budget / columns)));
    return vec2f(columns, rows);
}

// The source pixel shown at `destination`, clamped to the visible rect.
fn source_pixel_at(destination: vec2f) -> vec2f {
    let local = (destination - params.draw_rect.xy) / params.draw_rect.zw;
    var source_local = local;
    // Undo the clockwise turn, then the mirror that came before it.
    switch params.orientation & 3u {
        case 1u: { source_local = vec2f(local.y, 1.0 - local.x); }
        case 2u: { source_local = vec2f(1.0 - local.x, 1.0 - local.y); }
        case 3u: { source_local = vec2f(1.0 - local.y, local.x); }
        default: {}
    }
    if (params.orientation & 4u) != 0u {
        source_local.x = 1.0 - source_local.x;
    }
    let visible_origin = params.visible_rect.xy;
    let visible_size = params.visible_rect.zw;
    return clamp(
        visible_origin + source_local * visible_size,
        visible_origin + vec2f(0.5),
        visible_origin + visible_size - vec2f(0.5),
    );
}

// One position as the intermediate stores it: premultiplied, sRGB-encoded.
fn stored_sample(source_pixel: vec2f) -> vec4f {
    return finish(filtered_sample(source_pixel));
}

// One premultiplied bilinear sample, sRGB-encoded for modes 0 and 1, linear for mode 2.
fn filtered_sample(source_pixel: vec2f) -> vec4f {
    // Control flow is non-uniform here, so sample level 0 explicitly (there are no mips).
    let source = textureSampleLevel(
        source_texture,
        source_sampler,
        source_pixel / params.output_source_size.zw,
        0.0,
    );
    if params.mode != 0u {
        return source;
    }
    // Premultiply straight alpha before filtering, or transparent texels darken edges. When all
    // four texels are opaque the hardware sample is used as is.
    let texel = source_pixel - vec2f(0.5);
    let origin = vec2i(floor(texel));
    let weight = texel - floor(texel);
    let last = vec2i(textureDimensions(source_texture)) - vec2i(1);
    let t00 = premultiplied_texel(clamp(origin, vec2i(0), last));
    let t10 = premultiplied_texel(clamp(origin + vec2i(1, 0), vec2i(0), last));
    let t01 = premultiplied_texel(clamp(origin + vec2i(0, 1), vec2i(0), last));
    let t11 = premultiplied_texel(clamp(origin + vec2i(1, 1), vec2i(0), last));
    if min(min(t00.a, t10.a), min(t01.a, t11.a)) >= 1.0 {
        return vec4f(source.rgb * source.a, source.a);
    }
    return mix(mix(t00, t10, weight.x), mix(t01, t11, weight.x), weight.y);
}

// A filtered sample as the intermediate stores it.
fn finish(filtered: vec4f) -> vec4f {
    if params.mode != 2u {
        return filtered;
    }
    // Filtered in linear light, where premultiplied color averages as light does.
    if filtered.a <= 0.0 {
        return vec4f(0.0);
    }
    let straight = filtered.rgb / filtered.a;
    return vec4f(
        vec3f(
            extended_srgb(straight.r),
            extended_srgb(straight.g),
            extended_srgb(straight.b),
        ) * filtered.a,
        filtered.a,
    );
}

// The sRGB curve, odd-extended below zero and continued above one so out-of-range values survive.
fn extended_srgb(value: f32) -> f32 {
    let magnitude = abs(value);
    var encoded = 12.92 * magnitude;
    if magnitude > 0.0031308 {
        encoded = 1.055 * pow(magnitude, 1.0 / 2.4) - 0.055;
    }
    return sign(value) * encoded;
}

fn premultiplied_texel(position: vec2i) -> vec4f {
    let texel = textureLoad(source_texture, position, 0);
    return vec4f(texel.rgb * texel.a, texel.a);
}
