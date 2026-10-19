// Port of gl-transitions/transitions/wipeLeft.glsl
// Author: Jake Nelson
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    return mix(gl_from_color(uv), gl_to_color(uv), builtin_aa_step(1.0 - uv.x, progress()));
}
