// Port of gl-transitions/transitions/x_axis_translation.glsl
// Author: lizhongjian
// License: MIT
// alpha2: the edge of the shifted picture is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let shifted = builtin_aa_step(progress(), uv.x);
    let new_uv = vec2f(uv.x - progress(), uv.y);
    let faded = mix(gl_from_color(uv), gl_to_color(uv), progress());
    return mix(faded, gl_from_color(new_uv), shifted);
}
