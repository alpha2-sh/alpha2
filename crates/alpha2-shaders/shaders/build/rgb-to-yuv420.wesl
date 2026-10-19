import package::fullscreen::{VertexOutput, fullscreen_vertex};

@group(0) @binding(0)
var source_texture: texture_2d<f32>;

struct YuvParams {
    coefficients: vec4f,
    range: vec4f,
    // x: chroma co-sited with the even luma columns (1) or between them (0);
    // y: the same for rows.
    siting: vec4f,
};

@group(0) @binding(1)
var<uniform> params: YuvParams;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex(index);
}

// The output has no alpha, so it is dropped and premultiplied RGB is kept.
// The engine composites the project background under every layer first, so a
// frame arrives here opaque. Dividing alpha back out would show a translucent
// layer at full strength and erase its opacity and fades.
fn opaque_rgb(position: vec2i) -> vec3f {
    let source = textureLoad(source_texture, position, 0);
    return clamp(source.rgb, vec3f(0.0), vec3f(1.0));
}

fn yuv(rgb: vec3f) -> vec3f {
    let kr = params.coefficients.x;
    let kg = params.coefficients.y;
    let kb = params.coefficients.z;
    let luma = dot(rgb, vec3f(kr, kg, kb));
    let y = params.range.x + params.range.y * luma;
    let u = params.range.z + params.range.w * (rgb.b - luma) / (2.0 * (1.0 - kb));
    let v = params.range.z + params.range.w * (rgb.r - luma) / (2.0 * (1.0 - kr));
    return vec3f(y, u, v);
}

@fragment
fn y_fs(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let value = yuv(opaque_rgb(vec2i(position.xy))).x;
    return vec4f(value, 0.0, 0.0, 1.0);
}

struct ChromaOutput {
    @location(0) u: vec4f,
    @location(1) v: vec4f,
};

// NV12 interleaves U and V into one half-size two-channel plane, which is what
// a Vulkan encoder's `G8_B8R8_2PLANE_420_UNORM` image carries. The chroma
// samples are the same `chroma` average `uv_fs` computes; only the attachment
// they land in differs, so the two paths cannot drift apart.
@fragment
fn uv_nv12_fs(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let converted = chroma(position);
    return vec4f(converted.y, converted.z, 0.0, 1.0);
}

fn clamped_rgb(position: vec2i) -> vec3f {
    let last = vec2i(textureDimensions(source_texture)) - vec2i(1);
    return opaque_rgb(clamp(position, vec2i(0), last));
}

// How much luma sample `offset` from a chroma sample's even column (or row)
// weighs in it: co-sited, 1:2:1 around it; centred, the pair it sits between.
fn tap_weight(offset: i32, cosited: bool) -> f32 {
    if cosited { return select(0.25, 0.5, offset == 0); }
    return select(0.0, 0.5, offset >= 0);
}

// One chroma sample where the output's siting puts it: the RGB around that
// point, filtered for the 2:1 subsampling on each axis. Centred chroma sits
// between luma columns 2j and 2j+1 and is their average; co-sited chroma sits
// on column 2j and weighs 2j-1, 2j, 2j+1 as 1:2:1, the edge repeated. Rows
// take the same choice from `siting.y`.
fn chroma(position: vec4f) -> vec3f {
    let output = vec2i(position.xy);
    let source_origin = output * 2;
    if params.siting.x == 0.0 && params.siting.y == 0.0 {
        let rgb = (
            opaque_rgb(source_origin)
            + opaque_rgb(source_origin + vec2i(1, 0))
            + opaque_rgb(source_origin + vec2i(0, 1))
            + opaque_rgb(source_origin + vec2i(1, 1))
        ) * 0.25;
        return yuv(rgb);
    }
    var rgb = vec3f(0.0);
    for (var row = -1; row <= 1; row++) {
        let row_weight = tap_weight(row, params.siting.y != 0.0);
        if row_weight == 0.0 { continue; }
        for (var column = -1; column <= 1; column++) {
            let column_weight = tap_weight(column, params.siting.x != 0.0);
            if column_weight == 0.0 { continue; }
            rgb += clamped_rgb(source_origin + vec2i(column, row)) * (row_weight * column_weight);
        }
    }
    return yuv(rgb);
}

@fragment
fn uv_fs(@builtin(position) position: vec4f) -> ChromaOutput {
    let converted = chroma(position);
    return ChromaOutput(
        vec4f(converted.y, 0.0, 0.0, 1.0),
        vec4f(converted.z, 0.0, 0.0, 1.0),
    );
}
