// Heckbert (1989): the projective map of the unit square onto the quad, inverted by its
// adjugate. The source's uv square goes to the four output corners.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let p0 = params.top_left;
    let p1 = params.top_right;
    let p2 = params.bottom_right;
    let p3 = params.bottom_left;
    let d1 = p1 - p2;
    let d2 = p3 - p2;
    let d3 = p0 - p1 + p2 - p3;
    let det = d1.x * d2.y - d2.x * d1.y;
    // No early return: `params` is a private variable, so a branch on it is non-uniform to
    // Tint, and fwidthFine below must run in uniform control flow.
    let degenerate = abs(det) < 1.0e-8;
    let safe = select(det, 1.0, degenerate);
    let g = (d3.x * d2.y - d2.x * d3.y) / safe;
    let h = (d1.x * d3.y - d3.x * d1.y) / safe;
    let a = p1.x - p0.x + g * p1.x;
    let b = p3.x - p0.x + h * p3.x;
    let c = p0.x;
    let d = p1.y - p0.y + g * p1.y;
    let e = p3.y - p0.y + h * p3.y;
    let f = p0.y;
    // Adjugate of [[a, b, c], [d, e, f], [g, h, 1]].
    let ia = e - f * h;
    let ib = c * h - b;
    let ic = b * f - c * e;
    let id = f * g - d;
    let ie = a - c * g;
    let if_ = c * d - a * f;
    let ig = d * h - e * g;
    let ih = b * g - a * h;
    let ii = a * e - b * d;
    // The map's determinant: the adjugate gives each point of the square this weight's sign.
    let weight = a * ia + b * id + c * ig;
    let w = ig * uv.x + ih * uv.y + ii;
    let front = w * weight > 0.0 && !degenerate;
    let source = vec2f(ia * uv.x + ib * uv.y + ic, id * uv.x + ie * uv.y + if_) / select(1.0, w, front);
    let width = fwidthFine(source);
    // Behind the projection's horizon the map has no point of the square.
    return select(vec4f(0.0), builtin_sample_edge(source, width, 0u), front);
}
