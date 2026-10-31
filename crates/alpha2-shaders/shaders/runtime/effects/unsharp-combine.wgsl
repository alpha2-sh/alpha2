// Unsharp mask's last pass: the source (`from`) pushed from its blur (`to`) by `amount` where
// detail luminance passes `threshold`, compared as straight colors. Progress is ignored.
fn transition(uv: vec2f) -> vec4f {
    let src = from_color(uv);
    if src.a <= 0.0001 { return src; }
    let blurred = to_color(uv);
    let center = src.rgb / src.a;
    let around = select(center, blurred.rgb / max(blurred.a, 0.0001), blurred.a > 0.0001);
    let detail = center - around;
    let weight = builtin_smoothstep(0.0, params.threshold, abs(builtin_luminance(detail)));
    return builtin_output(center + detail * params.amount * weight, src.a);
}
