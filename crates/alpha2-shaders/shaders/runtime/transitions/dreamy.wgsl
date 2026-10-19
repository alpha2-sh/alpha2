// Port of gl-transitions/transitions/Dreamy.glsl
// Author: mikolalysenko
// License: MIT
fn dreamy_offset(phase: f32, x: f32) -> vec2f {
    let shift_y = 0.03 * phase * cos(10.0 * (phase + x));
    return vec2f(0.0, shift_y);
}

fn gl_transition(uv: vec2f) -> vec4f {
    return mix(
        gl_from_color(uv + dreamy_offset(progress(), uv.x)),
        gl_to_color(uv + dreamy_offset(1.0 - progress(), uv.x)),
        progress(),
    );
}
