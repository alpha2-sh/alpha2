// Port of gl-transitions/transitions/CircleCrop.glsl
// Author: fkuteken
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let ratio_scale = vec2f(1.0, 1.0 / ratio());
    let radius = pow(2.0 * abs(progress() - 0.5), 3.0);
    let distance_from_center = length((uv - vec2f(0.5)) * ratio_scale);
    let image = select(gl_from_color(uv), gl_to_color(uv), progress() >= 0.5);
    let coverage = (1.0 - builtin_aa_step(radius, distance_from_center)) * select(0.0, 1.0, radius > 0.0);
    return mix(builtin_premultiply(params.back_color), image, coverage);
}
