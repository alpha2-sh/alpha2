// Port of gl-transitions/transitions/zoomInOut.glsl
// Author: OllyOllyOlly
// License: MIT
fn zoom_uv(uv: vec2f, amount: f32) -> vec2f {
    return vec2f(0.5) + (uv - vec2f(0.5)) * (1.0 - amount);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let zoom_from = builtin_smoothstep(0.0, 1.0, progress() * 2.0);
    let zoom_to = builtin_smoothstep(0.0, 1.0, (1.0 - progress()) * 2.0);
    let crossfade = builtin_smoothstep(0.4, 0.6, progress());
    return mix(gl_from_color(zoom_uv(uv, zoom_from)), gl_to_color(zoom_uv(uv, zoom_to)), crossfade);
}
