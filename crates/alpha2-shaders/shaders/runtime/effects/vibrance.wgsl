// Saturation from the luminance, scaled more where the color is less saturated.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let saturation = max(rgb.r, max(rgb.g, rgb.b)) - min(rgb.r, min(rgb.g, rgb.b));
    let gray = vec3f(builtin_luminance(rgb));
    let scale = 1.0 + params.amount * (1.0 - saturation);
    return builtin_output(mix(gray, rgb, scale), src.a);
}
