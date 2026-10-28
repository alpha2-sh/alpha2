// The gray of each pixel mapped through three colors, the midtone at `midpoint`.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let t = builtin_gray(rgb);
    let m = params.midpoint;
    var mapped = mix(params.midtones.rgb, params.highlights.rgb, (t - m) / (1.0 - m));
    if t < m { mapped = mix(params.shadows.rgb, params.midtones.rgb, t / m); }
    return builtin_output(mix(rgb, mapped, params.amount), src.a);
}
