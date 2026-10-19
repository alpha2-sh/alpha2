// Port of gl-transitions/transitions/fadecolor.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let color = builtin_premultiply(params.color);
    let from_mix = mix(color, gl_from_color(uv), 1.0 - builtin_smoothstep(0.0, 1.0 - params.color_phase, progress()));
    let to_mix = mix(color, gl_to_color(uv), builtin_smoothstep(params.color_phase, 1.0, progress()));
    return mix(from_mix, to_mix, progress());
}
