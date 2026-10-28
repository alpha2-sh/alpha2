// Fractal value noise of up to four octaves, weighted to zero rather than skipped, so the cost is
// fixed and the control flow uniform.
fn turbulent_fbm(p: vec2f, seed: u32) -> f32 {
    let octaves = round(params.complexity);
    let w1 = select(0.0, 0.5, octaves >= 2.0);
    let w2 = select(0.0, 0.25, octaves >= 3.0);
    let w3 = select(0.0, 0.125, octaves >= 4.0);
    var sum = builtin_value_noise(p, seed);
    sum += w1 * builtin_value_noise(p * 2.03 + vec2f(17.0, 3.0), seed);
    sum += w2 * builtin_value_noise(p * 4.01 + vec2f(5.0, 29.0), seed);
    sum += w3 * builtin_value_noise(p * 7.97 + vec2f(41.0, 11.0), seed);
    return sum / (1.0 + w1 + w2 + w3);
}

// Each pixel displaced by two noise fields; `evolution` slides them over the picture with time
// (a translation, not a third noise dimension), its offset kept below 100 lattice cells.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let seed = builtin_seed(params.seed);
    let aspect = builtin_aspect();
    let drift = fract(time() * params.evolution * 0.01) * 100.0;
    let p = (uv - 0.5) * aspect / params.size + vec2f(drift);
    let d = vec2f(turbulent_fbm(p, seed), turbulent_fbm(p + vec2f(31.7, 47.3), seed + 1u)) * 2.0 - 1.0;
    let source = uv + d * params.amount / aspect;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
