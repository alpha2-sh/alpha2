// The render graph expands this effect into internal passes, so this body is
// never drawn; it only gives the public definition a module to register.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    return src;
}
