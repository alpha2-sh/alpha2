struct VertexOutput {
    @builtin(position) position: vec4f,
    @location(0) uv: vec2f,
}

fn fullscreen_vertex(index: u32) -> VertexOutput {
    let positions = array<vec2f, 3>(
        vec2f(-1.0, -1.0),
        vec2f(3.0, -1.0),
        vec2f(-1.0, 3.0),
    );
    let coordinates = array<vec2f, 3>(
        vec2f(0.0, 0.0),
        vec2f(2.0, 0.0),
        vec2f(0.0, 2.0),
    );
    return VertexOutput(vec4f(positions[index], 0.0, 1.0), coordinates[index]);
}

// Same triangle, v axis flipped, for passes that sample a source texture whose
// origin is top-left. Kept distinct from fullscreen_vertex: swapping the two
// mirrors the output vertically.
fn fullscreen_vertex_flipped(index: u32) -> VertexOutput {
    let positions = array<vec2f, 3>(
        vec2f(-1.0, -1.0),
        vec2f(3.0, -1.0),
        vec2f(-1.0, 3.0),
    );
    let coordinates = array<vec2f, 3>(
        vec2f(0.0, 1.0),
        vec2f(2.0, 1.0),
        vec2f(0.0, -1.0),
    );
    return VertexOutput(vec4f(positions[index], 0.0, 1.0), coordinates[index]);
}
