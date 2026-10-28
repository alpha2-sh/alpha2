// Mirrored wedges about `center`: each angle folded into the first half segment. The fold is
// continuous, so no seam shows; at the center, where the angle jumps, the radius is zero.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    let p = (uv - params.center) * aspect;
    let segment = BUILTIN_TAU / max(round(params.segments), 2.0);
    let turn = radians(params.angle);
    var a = atan2(p.y, p.x) - turn;
    a = a - segment * floor(a / segment);
    a = min(a, segment - a);
    let q = vec2f(cos(a), sin(a)) * length(p);
    let source = params.center + builtin_rotate(q, turn) / aspect;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
