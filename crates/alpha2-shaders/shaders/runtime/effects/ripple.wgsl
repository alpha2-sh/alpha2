// Rings travelling out of `center`, displacing radially and fading with distance. Cycles are
// reduced to their fraction because large-argument `sin` is imprecise.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    let p = (uv - params.center) * aspect;
    let r = length(p);
    let cycles = fract(params.speed * time());
    let wave = sin(BUILTIN_TAU * (r / params.wavelength - cycles)) * exp(-params.decay * r);
    let q = p + p / max(r, 0.000001) * params.amplitude * wave;
    let source = params.center + q / aspect;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
