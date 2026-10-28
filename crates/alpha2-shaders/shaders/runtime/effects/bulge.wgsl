// A radial map r' = r (1 - k (1 - x^2)^2), x = r / radius, k = 0.75 strength: positive strength
// magnifies the middle, negative pinches it. For |k| <= 0.75 the map never folds back (its slope
// stays at 0.25 or more), and at the rim it and its slope meet the identity.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    let p = (uv - params.center) * aspect;
    let x = length(p) / max(params.radius, 0.0001);
    let f = max(1.0 - x * x, 0.0);
    let q = p * (1.0 - params.strength * 0.75 * f * f);
    let source = params.center + q / aspect;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
