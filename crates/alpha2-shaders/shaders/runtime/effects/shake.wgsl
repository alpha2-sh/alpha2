// The whole frame moved, turned and zoomed by smooth noise over time, the same for every pixel.
// The noise lattice takes `time * frequency` as an integer cell, exact up to 2^24: at 30 Hz,
// about 155 hours.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let seed = builtin_seed(params.seed);
    let t = time() * params.frequency;
    let shift = vec2f(builtin_value_noise(vec2f(t, 0.5), seed), builtin_value_noise(vec2f(t, 3.5), seed)) * 2.0 - 1.0;
    let turn = (builtin_value_noise(vec2f(t, 6.5), seed) * 2.0 - 1.0) * radians(params.rotation);
    let aspect = builtin_aspect();
    let p = builtin_rotate((uv - 0.5) * aspect / params.zoom, -turn) - shift * params.amplitude;
    let source = p / aspect + 0.5;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
