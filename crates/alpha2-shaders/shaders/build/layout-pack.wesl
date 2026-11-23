import package::fullscreen::{VertexOutput, fullscreen_vertex};

@group(0) @binding(0)
var source_texture: texture_2d<f32>;

// Mirrors `layout_pack::Coding` and the CPU pack's chroma taps.
struct LayoutParams {
    kr: f32,
    kg: f32,
    kb: f32,
    maximum: f32,
    luma_black: f32,
    luma_span: f32,
    chroma_center: f32,
    chroma_span: f32,
    step_x: u32,
    step_y: u32,
    cosited_x: u32,
    cosited_y: u32,
    straight: u32,
    code_shift: u32,
    padding: vec2u,
};

@group(0) @binding(1)
var<uniform> params: LayoutParams;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex(index);
}

fn texel(position: vec2i) -> vec4f {
    return textureLoad(source_texture, position, 0);
}

fn code(value: f32) -> u32 {
    return u32(round(clamp(value, 0.0, params.maximum))) << params.code_shift;
}

fn luma(rgb: vec3f) -> f32 {
    return rgb.r * params.kr + rgb.g * params.kg + rgb.b * params.kb;
}

fn luma_code(rgb: vec3f) -> u32 {
    return code(params.luma_black + params.luma_span * luma(rgb));
}

fn chroma_pair(rgb: vec3f) -> vec2u {
    let y = luma(rgb);
    return vec2u(
        code(params.chroma_center + params.chroma_span * (rgb.b - y) / (2.0 * (1.0 - params.kb))),
        code(params.chroma_center + params.chroma_span * (rgb.r - y) / (2.0 * (1.0 - params.kr))),
    );
}

fn straight() -> bool {
    return params.straight != 0u;
}

fn packed_color(source: vec4f) -> vec3f {
    if !straight() {
        return clamp(source.rgb, vec3f(0.0), vec3f(1.0));
    }
    let alpha = clamp(source.a, 0.0, 1.0);
    if alpha <= 0.0 {
        return vec3f(0.0);
    }
    return clamp(source.rgb / alpha, vec3f(0.0), vec3f(1.0));
}

fn tap_weight(offset: i32, step: u32, cosited: bool) -> f32 {
    if step == 1u {
        return select(0.0, 1.0, offset == 0);
    }
    if cosited {
        return select(0.25, 0.5, offset == 0);
    }
    return select(0.0, 0.5, offset >= 0);
}

fn chroma_codes(position: vec4f) -> vec2u {
    let output = vec2i(position.xy);
    let steps = vec2u(params.step_x, params.step_y);
    if all(steps == vec2u(1u)) {
        return chroma_pair(packed_color(texel(output)));
    }
    let origin = output * vec2i(steps);
    let last = vec2i(textureDimensions(source_texture)) - vec2i(1);
    var sum = vec3f(0.0);
    var weight_sum = 0.0;
    for (var row = -1; row <= 1; row++) {
        let row_weight = tap_weight(row, params.step_y, params.cosited_y != 0u);
        if row_weight == 0.0 { continue; }
        for (var column = -1; column <= 1; column++) {
            let column_weight = tap_weight(column, params.step_x, params.cosited_x != 0u);
            if column_weight == 0.0 { continue; }
            let source = texel(clamp(origin + vec2i(column, row), vec2i(0), last));
            let weight = row_weight * column_weight;
            if straight() {
                sum += clamp(source.rgb, vec3f(0.0), vec3f(1.0)) * weight;
                weight_sum += clamp(source.a, 0.0, 1.0) * weight;
            } else {
                sum += packed_color(source) * weight;
            }
        }
    }
    if !straight() {
        return chroma_pair(sum);
    }
    if weight_sum <= 0.0 {
        return vec2u(code(params.chroma_center));
    }
    return chroma_pair(clamp(sum / weight_sum, vec3f(0.0), vec3f(1.0)));
}

@fragment
fn luma_fs(@builtin(position) position: vec4f) -> @location(0) vec4u {
    return vec4u(luma_code(packed_color(texel(vec2i(position.xy)))), 0u, 0u, 0u);
}

struct LumaAlpha {
    @location(0) luma: vec4u,
    @location(1) alpha: vec4u,
};

@fragment
fn luma_alpha_fs(@builtin(position) position: vec4f) -> LumaAlpha {
    let source = texel(vec2i(position.xy));
    return LumaAlpha(
        vec4u(luma_code(packed_color(source)), 0u, 0u, 0u),
        vec4u(code(clamp(source.a, 0.0, 1.0) * params.maximum), 0u, 0u, 0u),
    );
}

struct ChromaPlanes {
    @location(0) cb: vec4u,
    @location(1) cr: vec4u,
};

@fragment
fn chroma_fs(@builtin(position) position: vec4f) -> ChromaPlanes {
    let codes = chroma_codes(position);
    return ChromaPlanes(vec4u(codes.x, 0u, 0u, 0u), vec4u(codes.y, 0u, 0u, 0u));
}

// `P010` interleaves Cb and Cr in one plane.
@fragment
fn chroma_interleaved_fs(@builtin(position) position: vec4f) -> @location(0) vec4u {
    let codes = chroma_codes(position);
    return vec4u(codes.x, codes.y, 0u, 0u);
}
