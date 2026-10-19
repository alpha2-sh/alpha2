// Port of gl-transitions/transitions/ripple.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let direction = uv - vec2f(0.5);
    let distance_from_center = length(direction);
    let offset = direction
        * (sin(progress() * distance_from_center * params.amplitude - progress() * params.speed) + 0.5)
        / 30.0
        * progress();
    return mix(
        gl_from_color(uv + offset),
        gl_to_color(uv),
        smoothstep(0.2, 1.0, progress()),
    );
}
