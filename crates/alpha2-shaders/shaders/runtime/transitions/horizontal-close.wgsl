// Port of gl-transitions/transitions/HorizontalClose.glsl
// Author: martiniti
// License: MIT
// alpha2: the descending ramp is the helper's, defined the same on every backend, and
// antialiased, so it stays smooth where it is narrower than a pixel.
fn gl_transition(uv: vec2f) -> vec4f {
    let s = 2.0 - abs((uv.y - 0.5) / (progress() - 1.0)) - 2.0 * progress();
    return mix(gl_from_color(uv), gl_to_color(uv), builtin_aa_smoothstep(0.5, 0.0, s));
}
