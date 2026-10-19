import package::fullscreen::{VertexOutput, fullscreen_vertex};

@group(0) @binding(0)
var source_texture: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    return fullscreen_vertex(index);
}

@fragment
fn fs() -> @location(0) vec4f {
    return textureLoad(source_texture, vec2i(0, 0), 0);
}
