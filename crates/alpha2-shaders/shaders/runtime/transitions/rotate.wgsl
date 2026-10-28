// Port of gl-transitions/transitions/rotateTransition.glsl
// Author: haiyoucuv
// License: MIT
// alpha2: pi to f32 precision rather than upstream's 3.1415926.
const ROTATE_PI = 3.14159265358979;

fn gl_transition(uv: vec2f) -> vec4f {
    let angle = progress() * ROTATE_PI * 2.0;
    let c = cos(angle);
    let s = sin(angle);
    // Upstream's row vector times mat2(c, -s, s, c).
    let v = uv - 0.5;
    let p = fract(vec2f(v.x * c - v.y * s, v.x * s + v.y * c) + 0.5);
    return mix(gl_from_color(p), gl_to_color(p), progress());
}
