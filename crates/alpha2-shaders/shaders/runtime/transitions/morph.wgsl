// Port of gl-transitions/transitions/morph.glsl
// Author: paniq
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let source = gl_from_color(uv);
    let destination = gl_to_color(uv);
    let source_color = builtin_straight(source);
    let target_color = builtin_straight(destination);
    let from_offset = select(vec2f(0.0), source_color.rg + vec2f(source_color.b) - vec2f(1.0), source.a > 0.000001);
    let to_offset = select(vec2f(0.0), target_color.rg + vec2f(target_color.b) - vec2f(1.0), destination.a > 0.000001);
    let offset = mix(from_offset, to_offset, 0.5) * params.strength;
    return mix(
        gl_from_color(uv + offset * progress()),
        gl_to_color(uv - offset * (1.0 - progress())),
        progress(),
    );
}
