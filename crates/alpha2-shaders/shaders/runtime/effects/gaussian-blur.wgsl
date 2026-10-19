// One axis of a Gaussian: taps `params.step` apart in uv, with a standard
// deviation of `params.sigma` taps. The graph chains these passes so each one
// reads a picture already smoothed wider than its tap spacing.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let sigma = max(params.sigma, 0.0001);
    let reach = min(i32(ceil(sigma * 3.0)), min(12, max(1, i32(floor(max_filter_samples() * 0.5)))));
    var result = vec4f(0.0);
    var weight_sum = 0.0;
    for (var index: i32 = -12; index <= 12; index++) {
        if (abs(index) > reach) { continue; }
        let position = f32(index) / sigma;
        let weight = exp(-0.5 * position * position);
        result += sample_src(uv + params.step * f32(index)) * weight;
        weight_sum += weight;
    }
    return result / weight_sum;
}
