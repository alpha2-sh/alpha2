// Port of gl-transitions/transitions/swap.glsl
// Author: gre
// License: MIT
// alpha2: the opaque black floor is `back_color`, the reflections are composited over it
// premultiplied, the card edges are antialiased and the divisions are guarded.

fn swap_project(p: vec2f) -> vec2f {
    return p * vec2f(1.0, -1.2) + vec2f(0.0, -0.02);
}

fn gl_transition(p: vec2f) -> vec4f {
    let from_size = mix(1.0, params.depth, progress());
    let from_persp = params.perspective * progress();
    let from_p = (p + vec2f(-0.0, -0.5)) * vec2f(
        from_size / max(1.0 - params.perspective * progress(), 0.0001),
        from_size / max(1.0 - from_size * from_persp * p.x, 0.0001),
    ) + vec2f(0.0, 0.5);
    let to_size = mix(1.0, params.depth, 1.0 - progress());
    let to_persp = params.perspective * (1.0 - progress());
    let to_p = (p + vec2f(-1.0, -0.5)) * vec2f(
        to_size / max(1.0 - params.perspective * (1.0 - progress()), 0.0001),
        to_size / max(1.0 - to_size * to_persp * (0.5 - p.x), 0.0001),
    ) + vec2f(1.0, 0.5);
    let from_r = swap_project(from_p);
    let to_r = swap_project(to_p);
    let from_w = fwidthFine(from_p);
    let to_w = fwidthFine(to_p);
    let from_in = builtin_interval_coverage(from_p.x, from_w.x) * builtin_interval_coverage(from_p.y, from_w.y);
    let to_in = builtin_interval_coverage(to_p.x, to_w.x) * builtin_interval_coverage(to_p.y, to_w.y);
    let from_r_in = builtin_interval_coverage(from_r.x, from_w.x) * builtin_interval_coverage(from_r.y, from_w.y * 1.2);
    let to_r_in = builtin_interval_coverage(to_r.x, to_w.x) * builtin_interval_coverage(to_r.y, to_w.y * 1.2);
    let reflected = gl_from_color(from_r) * (params.reflection * (1.0 - from_r.y) * from_r_in)
        + gl_to_color(to_r) * (params.reflection * (1.0 - to_r.y) * to_r_in);
    let floor_color = builtin_over(builtin_valid(reflected), builtin_premultiply(params.back_color));
    let from_card = gl_from_color(from_p);
    let to_card = gl_to_color(to_p);
    // The outgoing card is in front for the first half, the incoming one after.
    let from_front = mix(mix(floor_color, to_card, to_in), from_card, from_in);
    let to_front = mix(mix(floor_color, from_card, from_in), to_card, to_in);
    return builtin_valid(select(to_front, from_front, progress() < 0.5));
}
