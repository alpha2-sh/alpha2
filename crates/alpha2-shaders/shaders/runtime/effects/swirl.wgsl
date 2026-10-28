// The picture turned about `center`, most at the center and not at all from `radius` out. The
// turn goes with the square of the distance to the rim, so it and its slope end at zero there.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    let p = (uv - params.center) * aspect;
    let t = clamp(1.0 - length(p) / max(params.radius, 0.0001), 0.0, 1.0);
    let q = builtin_rotate(p, radians(params.angle) * t * t);
    let source = params.center + q / aspect;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
