// Draws a rendered frame onto a browser canvas: a full-screen triangle sampling
// the frame, stretched to the canvas.

@group(0) @binding(0) var source: texture_2d<f32>;
@group(0) @binding(1) var source_sampler: sampler;

struct VertexOutput {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
};

@vertex
fn vs(@builtin(vertex_index) index: u32) -> VertexOutput {
    let positions = array<vec2f, 3>(vec2f(-1.0, -1.0), vec2f(3.0, -1.0), vec2f(-1.0, 3.0));
    let uvs = array<vec2f, 3>(vec2f(0.0, 1.0), vec2f(2.0, 1.0), vec2f(0.0, -1.0));
    return VertexOutput(vec4f(positions[index], 0.0, 1.0), uvs[index]);
}

@fragment
fn fs(input: VertexOutput) -> @location(0) vec4f {
    return textureSample(source, source_sampler, input.uv);
}
