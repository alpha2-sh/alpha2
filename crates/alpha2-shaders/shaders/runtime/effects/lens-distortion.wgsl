// The Brown-Conrady radial model, r' = r (1 + k1 r^2 + k2 r^4), mapping each output position to
// the source position it shows, so no inverse is needed. Positive k1 is barrel distortion,
// negative pincushion.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    // Radius 1 at the corners, whatever the aspect.
    let corner = 0.5 * length(aspect);
    let p = (uv - 0.5) * aspect / corner;
    let r2 = dot(p, p);
    let q = p * (1.0 + params.k1 * r2 + params.k2 * r2 * r2) / params.zoom;
    let source = q * corner / aspect + 0.5;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
