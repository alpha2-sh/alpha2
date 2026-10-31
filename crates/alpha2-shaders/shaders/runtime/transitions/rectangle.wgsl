// Port of gl-transitions/transitions/Rectangle.glsl
// Author: martiniti
// License: MIT
// alpha2: bgcolor is bg_color, premultiplied, and the box edges are antialiased; box coverage
// replaces upstream's step(s, dist) and pow.
fn gl_transition(uv: vec2f) -> vec4f {
    let t = abs(1.0 - 2.0 * progress());
    let box_x = builtin_aa_step(t, uv.x + 0.25) * builtin_aa_step(t, 1.25 - uv.x);
    let box_y = builtin_aa_step(t, uv.y + 0.25) * builtin_aa_step(t, 1.25 - uv.y);
    let image = select(gl_to_color(uv), gl_from_color(uv), progress() < 0.5);
    return mix(image, builtin_premultiply(params.bg_color), box_x * box_y);
}
