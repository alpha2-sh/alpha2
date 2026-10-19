import package::fullscreen::{VertexOutput, fullscreen_vertex};

// Copies decoded 8-bit 4:2:0 planes sample for sample into NV12 targets. No
// color math: an 8-bit code read as unorm and written to an 8-bit unorm target
// rounds back to the same code.

@group(0) @binding(0)
var luma_plane: texture_2d<f32>;

// U, or interleaved UV.
@group(0) @binding(1)
var chroma_plane: texture_2d<f32>;

// V when chroma is planar; bound to the chroma plane otherwise and unread.
@group(0) @binding(2)
var v_plane: texture_2d<f32>;

struct RepackParams {
    // Luma origin of the visible picture in xy, its chroma origin in zw.
    origin: vec4u,
};

@group(0) @binding(3)
var<uniform> params: RepackParams;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex(index);
}

@fragment
fn luma_fs(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let texel = vec2u(position.xy) + params.origin.xy;
    return vec4f(textureLoad(luma_plane, texel, 0).r, 0.0, 0.0, 1.0);
}

@fragment
fn interleaved_chroma_fs(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let texel = vec2u(position.xy) + params.origin.zw;
    return vec4f(textureLoad(chroma_plane, texel, 0).rg, 0.0, 1.0);
}

@fragment
fn planar_chroma_fs(@builtin(position) position: vec4f) -> @location(0) vec4f {
    let texel = vec2u(position.xy) + params.origin.zw;
    let u = textureLoad(chroma_plane, texel, 0).r;
    let v = textureLoad(v_plane, texel, 0).r;
    return vec4f(u, v, 0.0, 1.0);
}
