// Port of gl-transitions/transitions/SimpleZoomOut.glsl
// Author: Tianshuo
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let quick = clamp(params.zoom_quickness, 0.2, 1.0);
    let amount = builtin_smoothstep(1.0 - quick, 1.0, progress());
    let zoomed = vec2f(0.5) + (uv - vec2f(0.5)) * (1.0 - (1.0 - amount));
    let cut = select(1.0, 0.0, progress() < 1.0 - quick);
    return mix(gl_from_color(uv), gl_to_color(zoomed), select(cut, amount, params.fade != 0u));
}
