// Removes the dark (key_out 0) or bright (key_out 1) pixels by their gray, `softness` wide
// ramps keeping part of them. The straight color is kept; only the alpha falls.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let gray = builtin_gray(rgb);
    let dark = builtin_smoothstep(params.threshold, params.threshold + params.softness, gray);
    let bright = 1.0 - builtin_smoothstep(1.0 - params.threshold - params.softness, 1.0 - params.threshold, gray);
    let keep = select(dark, bright, params.key_out == 1u);
    return builtin_output(rgb, src.a * keep);
}
