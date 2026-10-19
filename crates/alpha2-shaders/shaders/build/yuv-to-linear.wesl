import package::fullscreen::{VertexOutput, fullscreen_vertex_flipped};

struct YuvUniforms {
    row_r: vec4f,
    row_g: vec4f,
    row_b: vec4f,
    range_scale: vec4f,
    range_offset: vec4f,
    // 0: SDR, decoded with the sRGB EOTF. 1: PQ and 2: HLG, tone-mapped.
    transfer: u32,
    alpha: f32,
    // 1 when `alpha_plane` holds the source's straight alpha, which
    // `alpha_scale` brings to full range; 0 for an opaque source.
    alpha_plane_present: u32,
    alpha_scale: f32,
    output_source_size: vec4f,
    visible_rect: vec4f,
    draw_rect: vec4f,
    chroma_size_offset: vec4f,
    // Clockwise quarter turns in bits 0-1; bit 2 mirrors the stored picture
    // left to right before it turns.
    orientation: u32,
    plane_layout: u32,
    // Chroma samples per luma sample on each axis: 1/2 for 4:2:0.
    chroma_scale: vec2f,
    // Linear light from the source's primaries into BT.709, as rows.
    gamut_r: vec4f,
    gamut_g: vec4f,
    gamut_b: vec4f,
    // Source peak nits, SDR white nits, and their PQ codes.
    tone: vec4f,
    // x: HLG system gamma at the source peak.
    hlg: vec4f,
    // The chroma samples of the visible rect, in chroma sample positions:
    // xy the first sample's centre, zw the last one's. A decoder's surface
    // can be larger than its picture (1088 rows for 1080), and the chroma
    // beyond the picture is padding the encoder or decoder left in any state.
    chroma_window: vec4f,
    // x: the renderer's quality budget of samples per output pixel, which
    // caps the minification filter as it caps the composite's.
    filtering: vec4f,
}
// Planes are normalized textures the sampler filters (R8/RG8, or R16/RG16
// where the device has 16-bit normalized formats). Built with `u16_planes`,
// they are the decoder's 16-bit containers as unsigned integers (R16Uint/
// RG16Uint), which every backend samples, browser WebGPU and WebGL2
// included; this module filters and normalizes them itself, so no device
// converts samples on the CPU. `@if` comes last: wesl 0.1.2 drops every
// attribute after an `@if` that holds.
@group(0) @binding(0) @if(!u16_planes) var y_plane: texture_2d<f32>;
@group(0) @binding(1) @if(!u16_planes) var u_plane: texture_2d<f32>;
@group(0) @binding(2) @if(!u16_planes) var v_plane: texture_2d<f32>;
@group(0) @binding(5) @if(!u16_planes) var alpha_plane: texture_2d<f32>;
@group(0) @binding(0) @if(u16_planes) var y_plane: texture_2d<u32>;
@group(0) @binding(1) @if(u16_planes) var u_plane: texture_2d<u32>;
@group(0) @binding(2) @if(u16_planes) var v_plane: texture_2d<u32>;
@group(0) @binding(5) @if(u16_planes) var alpha_plane: texture_2d<u32>;
@group(0) @binding(3) var plane_sampler: sampler;
@group(0) @binding(4) var<uniform> params: YuvUniforms;

// The planes at a normalized position, filtered bilinearly with the edge
// repeated, as `plane_sampler` does; a 16-bit container reads as
// `container / 65535`, as an R16Unorm texture samples.
@if(!u16_planes) fn sample_y(position: vec2f) -> f32 {
    return textureSampleLevel(y_plane, plane_sampler, position, 0.0).r;
}
@if(!u16_planes) fn sample_u(position: vec2f) -> vec2f {
    return textureSampleLevel(u_plane, plane_sampler, position, 0.0).rg;
}
@if(!u16_planes) fn sample_v(position: vec2f) -> f32 {
    return textureSampleLevel(v_plane, plane_sampler, position, 0.0).r;
}
@if(!u16_planes) fn sample_alpha(position: vec2f) -> f32 {
    return textureSampleLevel(alpha_plane, plane_sampler, position, 0.0).r;
}
@if(u16_planes) fn sample_y(position: vec2f) -> f32 {
    return bilinear_u16(y_plane, position).r;
}
@if(u16_planes) fn sample_u(position: vec2f) -> vec2f {
    return bilinear_u16(u_plane, position).rg;
}
@if(u16_planes) fn sample_v(position: vec2f) -> f32 {
    return bilinear_u16(v_plane, position).r;
}
@if(u16_planes) fn sample_alpha(position: vec2f) -> f32 {
    return bilinear_u16(alpha_plane, position).r;
}

@if(u16_planes) fn bilinear_u16(plane: texture_2d<u32>, position: vec2f) -> vec2f {
    let size = vec2i(textureDimensions(plane));
    let texel = position * vec2f(size) - vec2f(0.5);
    let origin = vec2i(floor(texel));
    let weight = texel - floor(texel);
    let last = size - vec2i(1);
    let t00 = vec2f(textureLoad(plane, clamp(origin, vec2i(0), last), 0).rg);
    let t10 = vec2f(textureLoad(plane, clamp(origin + vec2i(1, 0), vec2i(0), last), 0).rg);
    let t01 = vec2f(textureLoad(plane, clamp(origin + vec2i(0, 1), vec2i(0), last), 0).rg);
    let t11 = vec2f(textureLoad(plane, clamp(origin + vec2i(1, 1), vec2i(0), last), 0).rg);
    return mix(mix(t00, t10, weight.x), mix(t01, t11, weight.x), weight.y) / 65535.0;
}


// The working space is display-referred linear light, so every SDR input is
// linearized with the sRGB EOTF whatever its transfer tag says. Export encodes
// with the exact inverse, which makes an import-then-export without effects the
// identity. A BT.709 tag names the camera OETF, not how the signal is shown.
fn srgb_eotf(value: f32) -> f32 {
    let x = clamp(value, 0.0, 1.0);
    return select(pow((x + 0.055) / 1.055, 2.4), x / 12.92, x <= 0.04045);
}

// HDR into the SDR working space, as `alpha2_render::hdr` defines it: PQ by
// its EOTF, HLG by its inverse OETF and OOTF at the source peak, into BT.709,
// then the BT.2390 EETF on the largest component from the source peak to SDR
// white, which keeps hue. What BT.709 cannot hold is clipped at zero.
const PQ_M1: f32 = 0.1593017578125;
const PQ_M2: f32 = 78.84375;
const PQ_C1: f32 = 0.8359375;
const PQ_C2: f32 = 18.8515625;
const PQ_C3: f32 = 18.6875;

fn pq_eotf(code: f32) -> f32 {
    let e = pow(max(clamp(code, 0.0, 1.0), 1.0e-10), 1.0 / PQ_M2);
    let y = pow(max(e - PQ_C1, 0.0) / (PQ_C2 - PQ_C3 * e), 1.0 / PQ_M1);
    return y * 10000.0;
}

fn pq_inverse_eotf(nits: f32) -> f32 {
    let y = pow(max(clamp(nits / 10000.0, 0.0, 1.0), 1.0e-10), PQ_M1);
    return pow((PQ_C1 + PQ_C2 * y) / (1.0 + PQ_C3 * y), PQ_M2);
}

fn hlg_inverse_oetf(code: f32) -> f32 {
    let e = clamp(code, 0.0, 1.0);
    if e <= 0.5 { return e * e / 3.0; }
    return (exp((e - 0.55991073) / 0.17883277) + 0.28466892) / 12.0;
}

fn bt2390_eetf(nits: f32) -> f32 {
    let source = params.tone.z;
    let value = pq_inverse_eotf(min(nits, params.tone.x)) / source;
    let max_lum = params.tone.w / source;
    let knee = 1.5 * max_lum - 0.5;
    var mapped = value;
    if value >= knee && knee < 1.0 {
        let t = (value - knee) / (1.0 - knee);
        let t2 = t * t;
        let t3 = t2 * t;
        mapped = (2.0 * t3 - 3.0 * t2 + 1.0) * knee
            + (t3 - 2.0 * t2 + t) * (1.0 - knee)
            + (-2.0 * t3 + 3.0 * t2) * max_lum;
    }
    return pq_eotf(mapped * source);
}

fn to_bt709(rgb: vec3f) -> vec3f {
    return vec3f(dot(params.gamut_r.xyz, rgb), dot(params.gamut_g.xyz, rgb), dot(params.gamut_b.xyz, rgb));
}

fn hdr_to_working(encoded: vec3f) -> vec3f {
    var nits: vec3f;
    if params.transfer == 2u {
        let scene = vec3f(hlg_inverse_oetf(encoded.r), hlg_inverse_oetf(encoded.g), hlg_inverse_oetf(encoded.b));
        let luma = max(dot(vec3f(0.2627, 0.6780, 0.0593), scene), 0.0);
        nits = scene * params.tone.x * pow(max(luma, 1.0e-10), params.hlg.x - 1.0);
    } else {
        nits = vec3f(pq_eotf(encoded.r), pq_eotf(encoded.g), pq_eotf(encoded.b));
    }
    let converted = to_bt709(nits);
    let largest = max(converted.r, max(converted.g, converted.b));
    var scale = 0.0;
    if largest > 0.0 { scale = bt2390_eetf(largest) / largest; }
    return clamp(converted * scale / params.tone.y, vec3f(0.0), vec3f(1.0));
}

struct SampleCoordinates {
    luma: vec2f,
    chroma: vec2f,
}

// The stored luma position output position `destination` shows, kept
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

// Source pixels per output pixel along each output axis; a quarter turn
// swaps which source axis that is. As in the composite, up to 8 x 8 samples
// within the quality budget.
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
    let budget = clamp(params.filtering.x, 1.0, 64.0);
    let columns = min(wanted.x, budget);
    let rows = min(wanted.y, max(1.0, floor(budget / columns)));
    return vec2f(columns, rows);
}

// Linear light premultiplied by the source's own alpha at one position.
fn linear_sample(source_pixel: vec2f) -> vec4f {
    let coordinates = coordinates_at(source_pixel);
    if params.alpha_plane_present != 0u {
        return premultiplied_linear(coordinates);
    }
    return vec4f(linear_rgb(coordinates), 1.0);
}

// One position as the intermediate stores every presented source:
// sRGB-encoded, then premultiplied.
fn stored_sample(source_pixel: vec2f) -> vec4f {
    let color = linear_sample(source_pixel);
    if color.a <= 0.0 { return vec4f(0.0); }
    let straight = color.rgb / color.a;
    return vec4f(
        vec3f(encode_srgb(straight.r), encode_srgb(straight.g), encode_srgb(straight.b)) * color.a,
        color.a,
    );
}

// The picture at output position `destination`. Shrinking more than two
// source pixels into each output pixel, one sample would read 2 x 2 samples
// of a wider footprint and drop the rest: detail turns to moire or vanishes.
// The footprint is sampled on a grid and the stored values averaged, as the
// composite's minification filter averages them, so a picture shrunk here
// and one shrunk by a layer's scale come out alike. Up to 2x it is one
// bilinear sample.
fn filtered_stored(destination: vec2f) -> vec4f {
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

// Whether output position `destination` is inside the contained picture;
// outside it the output is transparent.
fn inside_draw_rect(destination: vec2f) -> bool {
    let draw_origin = params.draw_rect.xy;
    let draw_size = params.draw_rect.zw;
    return all(destination >= draw_origin) && all(destination < draw_origin + draw_size);
}

// The luma and chroma coordinates of `source_pixel`, a position in luma samples.
fn coordinates_at(source_pixel: vec2f) -> SampleCoordinates {
    let source_size = params.output_source_size.zw;
    let chroma_size = params.chroma_size_offset.xy;
    let chroma_offset = params.chroma_size_offset.zw;
    let chroma_pixel = clamp(
        (source_pixel + chroma_offset) * params.chroma_scale,
        params.chroma_window.xy,
        params.chroma_window.zw,
    );
    return SampleCoordinates(source_pixel / source_size, chroma_pixel / chroma_size);
}

// Sampled at level 0 explicitly: this runs after the fragment's early return
// for pixels outside the draw rectangle, which is non-uniform control flow, and
// Tint and WebKit reject implicit-derivative sampling there. The planes have
// no mips, so the level is the same.
fn linear_rgb(coordinates: SampleCoordinates) -> vec3f {
    let chroma = sample_u(coordinates.chroma);
    let u = chroma.r;
    let v = select(sample_v(coordinates.chroma), chroma.g, params.plane_layout == 1u);
    let codes = vec3f(sample_y(coordinates.luma), u, v);
    let yuv = codes * params.range_scale.xyz + params.range_offset.xyz;
    let encoded = vec3f(
        dot(params.row_r.xyz, yuv),
        dot(params.row_g.xyz, yuv),
        dot(params.row_b.xyz, yuv),
    );
    if params.transfer != 0u {
        return hdr_to_working(clamp(encoded, vec3f(0.0), vec3f(1.0)));
    }
    let decoded = vec3f(
        srgb_eotf(encoded.r),
        srgb_eotf(encoded.g),
        srgb_eotf(encoded.b),
    );
    // BT.709 sources take the identity here, exactly; wide-gamut SDR is converted and clipped.
    return clamp(to_bt709(decoded), vec3f(0.0), vec3f(1.0));
}

fn alpha_at(coordinates: SampleCoordinates) -> f32 {
    let code = sample_alpha(coordinates.luma);
    return clamp(code * params.alpha_scale, 0.0, 1.0);
}

// Linear light premultiplied by the source's own alpha. Straight alpha has to
// be premultiplied before it is filtered, or the color of transparent samples
// (usually black) bleeds into the edge of opaque ones: the four luma samples
// around the position are each converted and premultiplied, then filtered.
// Where all four are opaque the two orders agree, and the opaque path's own
// sample is kept.
fn premultiplied_linear(coordinates: SampleCoordinates) -> vec4f {
    let texel = coordinates.luma * params.output_source_size.zw - vec2f(0.5);
    let origin = floor(texel);
    let weight = texel - origin;
    let first = params.visible_rect.xy;
    let last = first + params.visible_rect.zw - vec2f(1.0);
    let c00 = coordinates_at(clamp(origin, first, last) + vec2f(0.5));
    let c10 = coordinates_at(clamp(origin + vec2f(1.0, 0.0), first, last) + vec2f(0.5));
    let c01 = coordinates_at(clamp(origin + vec2f(0.0, 1.0), first, last) + vec2f(0.5));
    let c11 = coordinates_at(clamp(origin + vec2f(1.0, 1.0), first, last) + vec2f(0.5));
    let a00 = alpha_at(c00);
    let a10 = alpha_at(c10);
    let a01 = alpha_at(c01);
    let a11 = alpha_at(c11);
    if min(min(a00, a10), min(a01, a11)) >= 1.0 {
        return vec4f(linear_rgb(coordinates), 1.0);
    }
    let t00 = vec4f(linear_rgb(c00) * a00, a00);
    let t10 = vec4f(linear_rgb(c10) * a10, a10);
    let t01 = vec4f(linear_rgb(c01) * a01, a01);
    let t11 = vec4f(linear_rgb(c11) * a11, a11);
    return mix(mix(t00, t10, weight.x), mix(t01, t11, weight.x), weight.y);
}

fn encode_srgb(x: f32) -> f32 {
    if x <= 0.0031308 { return 12.92 * x; }
    return 1.055 * pow(x, 1.0 / 2.4) - 0.055;
}

@fragment fn fs_srgb(@location(0) uv: vec2f) -> @location(0) vec4f {
    let destination = uv * params.output_source_size.xy;
    if !inside_draw_rect(destination) { return vec4f(0.0); }
    return filtered_stored(destination) * params.alpha;
}

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex_flipped(index);
}
