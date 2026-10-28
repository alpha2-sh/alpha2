// Every color but the hues near `hue` turned gray; alpha is kept.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let hsv = builtin_rgb_to_hsv(rgb);
    let hue_gap = abs(fract(hsv.x - params.hue / 360.0 + 0.5) - 0.5) * 360.0;
    let in_hue = 1.0 - builtin_smoothstep(params.range, params.range + params.softness, hue_gap);
    // A grey has no hue to keep.
    let keep = in_hue * builtin_smoothstep(0.05, 0.2, hsv.y);
    return builtin_output(mix(rgb, vec3f(builtin_gray(rgb)), params.amount * (1.0 - keep)), src.a);
}
