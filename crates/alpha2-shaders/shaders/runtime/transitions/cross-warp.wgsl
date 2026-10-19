// Port of gl-transitions/transitions/crosswarp.glsl
// Author: Eke Péter
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let phase = smoothstep(0.0, 1.0, progress() * 2.0 + uv.x - 1.0);
    let from_uv = (uv - vec2f(0.5)) * (1.0 - phase) + vec2f(0.5);
    let to_uv = (uv - vec2f(0.5)) * phase + vec2f(0.5);
    return mix(gl_from_color(from_uv), gl_to_color(to_uv), phase);
}
