// Port of gl-transitions/transitions/Fold.glsl
// Author: nwoeanhinnogaehr
// License: MIT
// alpha2: the fold line is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let unfolded = builtin_aa_step(progress(), uv.x);
    let a = gl_from_color((uv - vec2f(progress(), 0.0)) / vec2f(1.0 - progress(), 1.0));
    let b = gl_to_color(uv / vec2f(progress(), 1.0));
    return mix(b, a, unfolded);
}
