// Inverts the encoded straight color, as editing tools do.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    return builtin_output(mix(rgb, vec3f(1.0) - rgb, params.amount), src.a);
}
