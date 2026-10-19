// Port of gl-transitions/transitions/Radial.glsl
// Author: Xaychru
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let centered = uv * 2.0 - vec2f(1.0);
    let edge = atan2(centered.y, centered.x) - (progress() - 0.5) * 3.141592653589 * 2.5;
    return mix(
        gl_to_color(uv),
        gl_from_color(uv),
        builtin_aa_ramp(0.0, params.smoothness, edge, builtin_angle_width(uv - 0.5)),
    );
}
