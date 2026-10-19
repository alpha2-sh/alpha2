// Port of gl-transitions/transitions/circleopen.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let opening = params.opening != 0u;
    let phase = select(1.0 - progress(), progress(), opening);
    let mask = builtin_aa_smoothstep(
        -params.smoothness,
        0.0,
        1.414213562373 * distance(vec2f(0.5), uv) - phase * (1.0 + params.smoothness),
    );
    return mix(gl_from_color(uv), gl_to_color(uv), select(mask, 1.0 - mask, opening));
}
