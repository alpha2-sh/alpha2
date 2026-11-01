// Never drawn: the render graph expands this effect into internal passes.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    return src;
}
