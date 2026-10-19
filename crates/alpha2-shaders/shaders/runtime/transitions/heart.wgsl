// Port of gl-transitions/transitions/heart.glsl
// Author: gre
// License: MIT
fn in_heart(point: vec2f, center: vec2f, size: f32) -> f32 {
    if (size == 0.0) {
        return 0.0;
    }
    let offset = (point - center) / (1.6 * size);
    let curve = offset.x * offset.x + offset.y * offset.y - 0.3;
    return builtin_aa_step(curve * curve * curve, offset.x * offset.x * offset.y * offset.y * offset.y);
}

fn gl_transition(uv: vec2f) -> vec4f {
    return mix(gl_from_color(uv), gl_to_color(uv), in_heart(uv, vec2f(0.5, 0.4), progress()));
}
