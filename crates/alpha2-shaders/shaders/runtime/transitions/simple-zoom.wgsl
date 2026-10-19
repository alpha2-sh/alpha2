// Port of gl-transitions/transitions/SimpleZoom.glsl
// Author: 0gust1
// License: MIT
fn zoom_uv(uv: vec2f, amount: f32) -> vec2f {
    return vec2f(0.5) + (uv - vec2f(0.5)) * (1.0 - amount);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let quickness = clamp(params.zoom_quickness, 0.2, 1.0);
    let zoomed = zoom_uv(uv, smoothstep(0.0, quickness, progress()));
    return mix(
        gl_from_color(zoomed),
        gl_to_color(uv),
        smoothstep(quickness - 0.2, 1.0, progress()),
    );
}
