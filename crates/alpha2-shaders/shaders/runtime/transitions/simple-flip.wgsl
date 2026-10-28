// Port of gl-transitions/transitions/SimpleFlip.glsl
// Author: nwoeanhinnogaehr
// License: MIT
// alpha2: beside the card is `back_color` (upstream: opaque black), the card edge is
// antialiased, and the division is guarded where the card is edge-on.

fn gl_transition(uv: vec2f) -> vec4f {
    let half_width = abs(progress() - 0.5);
    let x = (uv.x - 0.5) / max(half_width, 0.0001) * 0.5 + 0.5;
    let card = select(gl_from_color(vec2f(x, uv.y)), gl_to_color(vec2f(x, uv.y)), progress() >= 0.5);
    let coverage = builtin_aa_step(abs(uv.x - 0.5), half_width);
    return mix(builtin_premultiply(params.back_color), card, coverage);
}
