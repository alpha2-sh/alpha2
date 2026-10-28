// Port of gl-transitions/transitions/cube.glsl
// Author: gre
// License: MIT
// alpha2: the opaque black floor is `back_color`, the reflections are composited over it
// premultiplied, and the face edges are antialiased.

// Upstream's `xskew` with its center at 0 or 1 (`x` is the distance from that side): the
// division is guarded where it reaches zero, far outside the face.
fn cube_skew(p: vec2f, persp: f32, x: f32) -> vec2f {
    return vec2f(p.x, (p.y - 0.5 * (1.0 - persp) * x) / max(1.0 + (persp - 1.0) * x, 0.0001));
}

fn cube_project(p: vec2f) -> vec2f {
    return p * vec2f(1.0, -1.2) + vec2f(0.0, -params.floating / 100.0);
}

fn gl_transition(op: vec2f) -> vec4f {
    let uz = params.unzoom * 2.0 * (0.5 - abs(0.5 - progress()));
    let p = -uz * 0.5 + (1.0 + uz) * op;
    let from_q = (p - vec2f(progress(), 0.0)) / vec2f(1.0 - progress(), 1.0);
    let from_p = cube_skew(from_q, 1.0 - mix(progress(), 0.0, params.persp), from_q.x);
    let to_q = p / vec2f(progress(), 1.0);
    let to_p = cube_skew(to_q, mix(pow(progress(), 2.0), 1.0, params.persp), 1.0 - to_q.x);
    let from_r = cube_project(from_p);
    let to_r = cube_project(to_p);
    let from_w = fwidthFine(from_p);
    let to_w = fwidthFine(to_p);
    let from_in = builtin_interval_coverage(from_p.x, from_w.x) * builtin_interval_coverage(from_p.y, from_w.y);
    let to_in = builtin_interval_coverage(to_p.x, to_w.x) * builtin_interval_coverage(to_p.y, to_w.y);
    let from_r_in = builtin_interval_coverage(from_r.x, from_w.x) * builtin_interval_coverage(from_r.y, from_w.y * 1.2);
    let to_r_in = builtin_interval_coverage(to_r.x, to_w.x) * builtin_interval_coverage(to_r.y, to_w.y * 1.2);
    let reflected = gl_from_color(from_r) * (params.reflection * (1.0 - from_r.y) * from_r_in)
        + gl_to_color(to_r) * (params.reflection * (1.0 - to_r.y) * to_r_in);
    let floor_color = builtin_over(builtin_valid(reflected), builtin_premultiply(params.back_color));
    let faces = mix(mix(floor_color, gl_to_color(to_p), to_in), gl_from_color(from_p), from_in);
    return builtin_valid(faces);
}
