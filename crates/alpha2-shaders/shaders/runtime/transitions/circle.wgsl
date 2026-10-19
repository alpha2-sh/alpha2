// Port of gl-transitions/transitions/circle.glsl
// Author: Fernando Kuteken
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let radius = sqrt(8.0) * abs(progress() - 0.5);
    let coverage = (1.0 - builtin_aa_step(radius, distance(uv, params.center))) * select(0.0, 1.0, radius > 0.0);
    let image = select(gl_from_color(uv), gl_to_color(uv), progress() >= 0.5);
    return mix(builtin_premultiply(params.back_color), image, coverage);
}
