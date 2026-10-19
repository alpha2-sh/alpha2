// Each crop line is feathered across `feather` on either side. A side cropped by
// nothing keeps the frame's own edge: feathering it would fade an edge that
// nothing cut. Crops that meet or cross leave nothing.
fn builtin_crop_edge(position: f32, line: f32, feather: f32, cut: bool) -> f32 {
    if !cut { return 1.0; }
    return smoothstep(line - feather, line + feather, position);
}

fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let feather = max(params.feather, 0.0001);
    let low = params.top_left;
    let high = vec2f(1.0) - params.bottom_right;
    if any(high <= low) { return vec4f(0.0); }
    let inside = builtin_crop_edge(uv.x, low.x, feather, low.x > 0.0)
        * builtin_crop_edge(-uv.x, -high.x, feather, high.x < 1.0)
        * builtin_crop_edge(uv.y, low.y, feather, low.y > 0.0)
        * builtin_crop_edge(-uv.y, -high.y, feather, high.y < 1.0);
    return src * inside;
}
