// Port of gl-transitions/transitions/pinwheel.glsl
// Author: Mr Speaker
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let circle_position = atan2(uv.y - 0.5, uv.x - 0.5) + progress() * params.speed;
    let period = 3.1415 / 4.0;
    let mask = builtin_aa_pulse(circle_position / period, clamp(progress() / period, 0.0, 1.0), builtin_angle_width(uv - 0.5) / period);
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
