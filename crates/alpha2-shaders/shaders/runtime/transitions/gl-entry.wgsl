// The bodies are ports of gl-transitions and keep its coordinates: uv (0, 0) is the
// bottom-left corner and y grows upward, so the entry point and the samplers flip y.
fn transition(uv: vec2f) -> vec4f {
    return gl_transition(vec2f(uv.x, 1.0 - uv.y));
}

fn gl_from_color(uv: vec2f) -> vec4f {
    return from_color(vec2f(uv.x, 1.0 - uv.y));
}

fn gl_to_color(uv: vec2f) -> vec4f {
    return to_color(vec2f(uv.x, 1.0 - uv.y));
}
