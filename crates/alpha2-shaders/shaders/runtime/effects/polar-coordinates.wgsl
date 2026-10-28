// rect_to_polar wraps the picture around the center, its top edge meeting there; polar_to_rect
// unwraps it again. `amount` blends from the identity.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let c = uv - 0.5;
    let radius = length(c) * 2.0;
    let around = vec2f(fract(atan2(c.x, -c.y) / BUILTIN_TAU + 0.5), radius);
    // The same angle cut at the top instead of the bottom, for the derivative across the cut.
    let turned = vec2f(atan2(-c.x, c.y) / BUILTIN_TAU, radius);
    let angle = (uv.x - 0.5) * BUILTIN_TAU;
    let unwrapped = vec2f(0.5) + vec2f(sin(angle), -cos(angle)) * uv.y * 0.5;
    let unrolled = params.mode == 1u;
    let source = mix(uv, select(around, unwrapped, unrolled), params.amount);
    let other = mix(uv, select(turned, unwrapped, unrolled), params.amount);
    // Where the angle jumps from one to zero the jump is no footprint: the cut at the top gives
    // the angle's width there.
    let width = fwidthFine(source);
    let other_width = fwidthFine(other);
    return builtin_sample_edge(source, vec2f(min(width.x, other_width.x), width.y), params.edge);
}
