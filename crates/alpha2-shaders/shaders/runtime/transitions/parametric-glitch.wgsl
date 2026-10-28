// Port of gl-transitions/transitions/parametric_glitch.glsl
// Author: Yoni Maltsman @friendlyspinach
// License: MIT
// alpha2: the spiral's phase comes from the straight color, so it does not depend on the
// alpha.

fn gl_transition(uv: vec2f) -> vec4f {
    let rgb = builtin_straight(gl_from_color(uv));
    let incoming = gl_to_color(uv);
    let sphere = rgb.r * rgb.r + rgb.g * rgb.g + rgb.b * rgb.b - 1.0;
    let spiral_x = cos(sphere - uv.x / (progress() + 0.01));
    let spiral_y = sin(sphere - uv.y / (progress() + 0.01));
    let st = vec2f(fract(params.amp_x * uv.x * spiral_x), fract(params.amp_y * uv.y * spiral_y));
    let diff = uv - st;
    return mix(gl_from_color(uv + progress() * diff), incoming, progress());
}
