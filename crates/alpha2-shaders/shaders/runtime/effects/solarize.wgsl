// Encoded channels above `threshold` inverted.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    return builtin_output(select(rgb, vec3f(1.0) - rgb, rgb > vec3f(params.threshold)), src.a);
}
