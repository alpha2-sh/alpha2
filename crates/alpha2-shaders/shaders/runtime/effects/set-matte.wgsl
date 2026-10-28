// The source kept where the `matte` input covers it: its alpha, or its luminance over black
// (BT.709 on linear light, times alpha, as a track matte reads it), optionally inverted.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let m = sample_matte(uv);
    let linear = builtin_decode3(clamp(builtin_straight(m), vec3f(0.0), vec3f(1.0)));
    let coverage = select(m.a, builtin_luminance(linear) * m.a, params.mode == 1u);
    let kept = select(coverage, 1.0 - coverage, params.invert != 0u);
    return src * clamp(kept, 0.0, 1.0);
}
