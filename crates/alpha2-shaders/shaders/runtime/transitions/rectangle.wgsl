// Port of gl-transitions/transitions/Rectangle.glsl
// Author: martiniti
// License: MIT
// alpha2: bgcolor is bg_color, premultiplied, and the box's edges are antialiased. Upstream's
// step(s, dist) is dist itself (s is 0 only where the box fills the frame), so the box's
// coverage replaces both and its pow goes.
fn gl_transition(uv: vec2f) -> vec4f {
    let t = abs(1.0 - 2.0 * progress());
    let box_x = builtin_aa_step(t, uv.x + 0.25) * builtin_aa_step(t, 1.25 - uv.x);
    let box_y = builtin_aa_step(t, uv.y + 0.25) * builtin_aa_step(t, 1.25 - uv.y);
    let image = select(gl_to_color(uv), gl_from_color(uv), progress() < 0.5);
    return mix(image, builtin_premultiply(params.bg_color), box_x * box_y);
}
