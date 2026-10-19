// Port of gl-transitions/transitions/fade.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    return mix(gl_from_color(uv), gl_to_color(uv), progress());
}
