// Kuwahara (1976): the mean of whichever of the four quadrants around the pixel varies least,
// so regions flatten while their edges stay.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let side = max(floor(sqrt(clamp(max_filter_samples(), 4.0, 64.0) * 0.25)), 1.0);
    let reach = params.radius / builtin_aspect();
    var best = src;
    var best_variance = 1.0e9;
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
        let variance = spread.r + spread.g + spread.b;
        if variance < best_variance {
            best_variance = variance;
            best = mean;
        }
    }
    return best;
}
