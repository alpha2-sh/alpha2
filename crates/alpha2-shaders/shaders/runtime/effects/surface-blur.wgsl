// A bilateral filter: a square of taps `radius` (output heights) out, each weighted by its
// distance and by how far its premultiplied color is from the pixel's, so edges, alpha ones
// too, stay. The grid is odd, the pixel itself a tap of weight one: 7 x 7 at 64 samples,
// 3 x 3 at 16.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let full = floor(sqrt(clamp(max_filter_samples(), 1.0, 64.0)) + 0.0001);
    let side = 2.0 * floor((full - 1.0) * 0.5) + 1.0;
    let count = u32(side);
    let reach = params.radius / builtin_aspect();
    let inverse_range = 1.0 / (2.0 * params.threshold * params.threshold);
    var sum = vec4f(0.0);
    var weight = 0.0;
    for (var y = 0u; y < count; y++) {
        for (var x = 0u; x < count; x++) {
            let o = (vec2f(f32(x), f32(y)) + 0.5) / side * 2.0 - 1.0;
            let s = sample_src(uv + o * reach);
            let d = s - src;
            let w = exp(-dot(o, o) * 2.0) * exp(-dot(d, d) * inverse_range);
            sum += s * w;
            weight += w;
        }
    }
    return sum / max(weight, 0.000001);
}
