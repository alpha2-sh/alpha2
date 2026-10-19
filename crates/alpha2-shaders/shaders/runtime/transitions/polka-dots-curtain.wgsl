// Port of gl-transitions/transitions/PolkaDotsCurtain.glsl
// Author: bobylito
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let cell_distance = distance(fract(uv * params.dots), vec2f(0.5));
    let center_distance = max(distance(uv, params.center), 0.000001);
    let mask = 1.0 - builtin_aa_step(progress() / center_distance, cell_distance);
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
