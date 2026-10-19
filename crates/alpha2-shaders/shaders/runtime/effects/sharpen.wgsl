// Unsharp mask on the straight color against the coverage-weighted color of the four
// neighbours, so a transparent neighbour does not count as black.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    if src.a <= 0.0001 { return src; }
    let texel = 1.0 / resolution();
    let neighbours = sample_src(uv + vec2f(texel.x, 0.0))
        + sample_src(uv - vec2f(texel.x, 0.0))
        + sample_src(uv + vec2f(0.0, texel.y))
        + sample_src(uv - vec2f(0.0, texel.y));
    let center = src.rgb / src.a;
    var ring = center;
    if neighbours.a > 0.0001 { ring = neighbours.rgb / neighbours.a; }
    let rgb = clamp(center + (center - ring) * params.amount, vec3f(0.0), vec3f(1.0));
    return vec4f(rgb * src.a, src.a);
}
