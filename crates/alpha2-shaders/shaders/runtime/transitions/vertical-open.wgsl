// Port of gl-transitions/transitions/VerticalOpen.glsl
// Author: martiniti
// License: MIT
// alpha2: the ramp is antialiased, so it stays smooth where it is narrower than a pixel.
fn gl_transition(uv: vec2f) -> vec4f {
    let regress = 1.0 - progress();
    let s = 2.0 - abs((uv.x - 0.5) / (regress - 1.0)) - 2.0 * regress;
    return mix(gl_from_color(uv), gl_to_color(uv), builtin_aa_smoothstep(0.0, 0.5, s));
}
