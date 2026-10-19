import package::fullscreen::{VertexOutput, fullscreen_vertex_flipped};

struct PresentationParams {
    output_source_size: vec4f,
    visible_rect: vec4f,
    draw_rect: vec4f,
    // Clockwise quarter turns in bits 0-1; bit 2 mirrors the stored picture
    // left to right before it turns.
    orientation: u32,
    // 0: straight sRGB-encoded color; 1: premultiplied sRGB-encoded color;
    // 2: premultiplied linear-light color, stored with the sRGB curve
    // extended beyond 0..1.
    mode: u32,
    // The renderer's quality budget of samples per output pixel, which caps
    // the minification filter as it caps the composite's.
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
    // Shrinking more than two source pixels into each output pixel, one
    // sample would read 2 x 2 texels of a wider footprint and drop the rest:
    // detail turns to moire or vanishes. As the composite does, sample the
    // footprint on a grid of up to 8 x 8 points within the quality budget,
    // and average the stored values, so a picture shrunk here and one shrunk
    // by a layer's scale come out alike. Up to 2x it is one bilinear sample.
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

// Source pixels per output pixel along each output axis; a quarter turn
// swaps which source axis that is.
fn filter_grid() -> vec2f {
    var extent = params.visible_rect.zw;
    if (params.orientation & 1u) != 0u {
        extent = extent.yx;
    }
    // One bilinear sample spans two texels, so up to twice as many source
    // pixels as output pixels it misses none: the grid starts past 2x.
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

// The stored pixel position output position `destination` shows, kept
// inside the visible rect.
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

// One position as the intermediate stores every presented source:
// premultiplied, sRGB-encoded.
fn stored_sample(source_pixel: vec2f) -> vec4f {
    return finish(filtered_sample(source_pixel));
}

// One bilinear sample at `source_pixel`, as premultiplied color in the space
// the texture is filtered in: sRGB-encoded for modes 0 and 1, linear light
// for mode 2.
fn filtered_sample(source_pixel: vec2f) -> vec4f {
    // The contain test above is fragment-varying, so implicit derivatives are
    // not valid here. This texture has a single mip level; explicitly sampling
    // level zero preserves bilinear filtering without requiring uniform flow.
    let source = textureSampleLevel(
        source_texture,
        source_sampler,
        source_pixel / params.output_source_size.zw,
        0.0,
    );
    if params.mode != 0u {
        return source;
    }
    // Straight alpha has to be premultiplied before it is filtered. Filtering
    // first lets the color of transparent texels (usually black) bleed into
    // the edge of the opaque ones, a dark fringe around every scaled
    // transparent edge. Where all four texels the filter reads are opaque the
    // two orders agree, and the hardware sample is kept as it is.
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

// The sRGB curve, odd-extended below zero and continued above one, so light
// beyond SDR white and out-of-gamut negatives survive storage.
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
