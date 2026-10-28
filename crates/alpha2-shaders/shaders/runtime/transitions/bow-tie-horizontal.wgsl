// Port of gl-transitions/transitions/BowTieHorizontal.glsl
// Author: huynx
// License: MIT
// alpha2: the triangles are signed distances antialiased on every edge, not a fixed 0.005 blur
// on two, and both open from the start (upstream holds the left one back until progress 0.1).

// Signed distance from `p` to the line through `a` and `b`, positive on the side `o` picks.
fn bow_tie_edge(p: vec2f, a: vec2f, b: vec2f, o: f32) -> f32 {
    let e = b - a;
    return (e.x * (p.y - a.y) - e.y * (p.x - a.x)) * o / max(length(e), 0.000001);
}

// Positive inside the triangle `abc` of either winding.
fn bow_tie_triangle(p: vec2f, a: vec2f, b: vec2f, c: vec2f) -> f32 {
    let o = select(-1.0, 1.0, (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x) >= 0.0);
    return min(min(bow_tie_edge(p, a, b, o), bow_tie_edge(p, b, c, o)), bow_tie_edge(p, c, a, o));
}

fn gl_transition(uv: vec2f) -> vec4f {
    let p = progress();
    let left = bow_tie_triangle(uv, vec2f(p, 0.5), vec2f(0.0, 0.5 - p), vec2f(0.0, 0.5 + p));
    let right = bow_tie_triangle(uv, vec2f(1.0 - p, 0.5), vec2f(1.0, 0.5 - p), vec2f(1.0, 0.5 + p));
    return mix(gl_from_color(uv), gl_to_color(uv), builtin_aa_step(0.0, max(left, right)));
}
