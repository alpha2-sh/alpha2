// Kuwahara (1976) with Papari, Petkov and Campisi (2007) weighting: quadrant means weighted by
// inverse fourth-power variance. Picking only the least varying quadrant would flicker between
// near ties.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let side = max(floor(sqrt(clamp(max_filter_samples(), 4.0, 64.0) * 0.25)), 1.0);
    let reach = params.radius / builtin_aspect();
    var weighted = vec4f(0.0);
    var total = 0.0;
    for (var q = 0u; q < 4u; q++) {
        let quadrant = vec2f(select(-1.0, 1.0, (q & 1u) == 1u), select(-1.0, 1.0, (q & 2u) == 2u));
        var sum = vec4f(0.0);
        var squares = vec3f(0.0);
        for (var y = 0u; y < u32(side); y++) {
            for (var x = 0u; x < u32(side); x++) {
                let o = (vec2f(f32(x), f32(y)) + 0.5) / side * quadrant;
                let s = sample_src(uv + o * reach);
                sum += s;
                squares += s.rgb * s.rgb;
            }
        }
        let n = side * side;
        let mean = sum / n;
        let spread = squares / n - mean.rgb * mean.rgb;
        let variance = max(spread.r + spread.g + spread.b, 0.0) + 1.0e-3;
        let quartic = (variance * variance) * (variance * variance);
        let weight = 1.0 / quartic;
        weighted += mean * weight;
        total += weight;
    }
    return weighted / total;
}
