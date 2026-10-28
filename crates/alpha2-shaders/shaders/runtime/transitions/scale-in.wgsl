// Port of gl-transitions/transitions/scale-in.glsl
// Author: haiyoucuv
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let scaled = vec2f(0.5) + (uv - vec2f(0.5)) * progress();
    return mix(gl_from_color(uv), gl_to_color(scaled), progress());
}
