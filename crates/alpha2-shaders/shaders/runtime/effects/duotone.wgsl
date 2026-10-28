// The gray of each pixel mapped from the shadow color to the highlight color.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let toned = mix(params.shadows.rgb, params.highlights.rgb, builtin_gray(rgb));
    return builtin_output(mix(rgb, toned, params.amount), src.a);
}
