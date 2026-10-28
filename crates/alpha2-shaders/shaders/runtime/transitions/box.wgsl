// Port of gl-transitions/transitions/Box.glsl
// Author: lql
// License: MIT
// alpha2: upstream's rectIn and location ints are a Bool and an Enum; the box's edges are
// antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let rect_in = params.rect_in != 0u;
    let p = select(progress(), 1.0 - progress(), rect_in);
    let centered = params.location == 0u;
    let left = params.location == 1u || params.location == 2u;
    let top = params.location == 1u || params.location == 3u;
    let inset = 0.5 * (1.0 - p);
    let x1 = select(select(1.0 - p, 0.0, left), inset, centered);
    let y1 = select(select(0.0, 1.0 - p, top), inset, centered);
    let x2 = select(select(1.0, p, left), 1.0 - inset, centered);
    let y2 = select(select(p, 1.0, top), 1.0 - inset, centered);
    let in_rect = builtin_aa_step(x1, uv.x) * builtin_aa_step(uv.x, x2)
        * builtin_aa_step(y1, uv.y) * builtin_aa_step(uv.y, y2);
    return mix(gl_from_color(uv), gl_to_color(uv), select(in_rect, 1.0 - in_rect, rect_in));
}
