// Gray from a weighted sum of the channels in linear light, encoded again; the default
// weights are the BT.709 luminance.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let weights = vec3f(params.red, params.green, params.blue);
    let gray = builtin_encode(clamp(dot(builtin_decode3(rgb), weights), 0.0, 1.0));
    return builtin_output(mix(rgb, vec3f(gray), params.amount), src.a);
}
