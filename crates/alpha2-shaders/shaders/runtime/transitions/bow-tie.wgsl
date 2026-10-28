// Port of gl-transitions/transitions/BowTieWithParameter.glsl
// Author: KMojek
// License: MIT
// alpha2: the triangles are signed distances, antialiased, instead of inside tests.

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
    let reversed = params.reversed != 0u;
    let prog = select(progress(), 1.0 - progress(), reversed);
    let first = select(progress() < 0.5, progress() >= 0.5, reversed);
    let height = 0.5;
    // The first half: the incoming picture closes in from the top and bottom. Each triangle
    // stays in its own half of the frame, so the two are one union.
    let bottom = bow_tie_triangle(
        uv,
        vec2f(0.0, prog - height),
        vec2f(1.0, prog - height),
        vec2f(params.adjust, prog),
    );
    let top = bow_tie_triangle(
        uv,
        vec2f(0.0, 1.0 - prog + height),
        vec2f(1.0, 1.0 - prog + height),
        vec2f(params.adjust, 1.0 - prog),
    );
    // The second half: what is left of the outgoing picture shrinks to the sides.
    let s = 2.0 * (prog - 0.5);
    let right = bow_tie_triangle(
        uv,
        vec2f(prog + height, 1.0),
        vec2f(prog + height, 0.0),
        vec2f(mix(params.adjust, 1.0, s), 0.5),
    );
    let left = bow_tie_triangle(
        uv,
        vec2f(1.0 - prog - height, 1.0),
        vec2f(1.0 - prog - height, 0.0),
        vec2f(mix(params.adjust, 0.0, s), 0.5),
    );
    let incoming = select(-max(right, left), max(bottom, top), first);
    return mix(gl_from_color(uv), gl_to_color(uv), builtin_aa_step(0.0, incoming));
}
