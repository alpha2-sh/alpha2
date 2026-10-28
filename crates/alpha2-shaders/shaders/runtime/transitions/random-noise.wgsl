// Port of gl-transitions/transitions/randomNoisex.glsl
// Author: towrabbit
// License: MIT
// alpha2: a PCG hash of the output pixel replaces upstream's `fract(sin(...))` of uv.
fn gl_transition(uv: vec2f) -> vec4f {
    let pixel = uv * resolution();
    let random = builtin_unit(builtin_hash3(builtin_cell(pixel.x), builtin_cell(pixel.y), 0u));
    let amount = floor(random + progress());
    return mix(gl_from_color(uv), gl_to_color(uv), amount);
}
