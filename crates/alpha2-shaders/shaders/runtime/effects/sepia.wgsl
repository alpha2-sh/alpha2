// The common sepia matrix on the encoded color; white passes one and is clamped.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let toned = vec3f(
        dot(rgb, vec3f(0.393, 0.769, 0.189)),
        dot(rgb, vec3f(0.349, 0.686, 0.168)),
        dot(rgb, vec3f(0.272, 0.534, 0.131)),
    );
    return builtin_output(mix(rgb, toned, params.amount), src.a);
}
