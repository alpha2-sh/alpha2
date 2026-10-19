// Port of gl-transitions/transitions/angular.glsl
// Author: Fernando Kuteken
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let pi = 3.141592653589;
    let offset = params.starting_angle * pi / 180.0;
    let angle = atan2(uv.y - 0.5, uv.x - 0.5) + offset;
    let normalized_angle = fract((angle + pi) / (2.0 * pi));
    let width = builtin_angle_width(uv - 0.5) / (2.0 * pi);
    let mask = builtin_aa_pulse(normalized_angle, progress(), width);
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
