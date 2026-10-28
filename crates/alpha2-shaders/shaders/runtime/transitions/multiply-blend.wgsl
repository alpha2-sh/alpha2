// Port of gl-transitions/transitions/multiply_blend.glsl
// Author: Fernando Kuteken
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let outgoing = gl_from_color(uv);
    let incoming = gl_to_color(uv);
    let blended = outgoing * incoming;
    if progress() < 0.5 {
        return mix(outgoing, blended, 2.0 * progress());
    }
    return mix(blended, incoming, 2.0 * progress() - 1.0);
}
