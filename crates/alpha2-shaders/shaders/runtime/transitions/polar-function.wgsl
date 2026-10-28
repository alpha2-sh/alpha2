// Port of gl-transitions/transitions/polar_function.glsl
// Author: Fernando Kuteken
// License: MIT
// alpha2: segments is rounded to a whole number, as upstream's int keeps the curve seamless;
// the edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let pi = 3.14159265359;
    let angle = atan2(uv.y - 0.5, uv.x - 0.5) - 0.5 * pi;
    let radius = (cos(round(params.segments) * angle) + 4.0) / 4.0;
    let difference = length(uv - vec2f(0.5, 0.5));
    let outside = builtin_aa_step(radius * progress(), difference);
    return mix(gl_to_color(uv), gl_from_color(uv), outside);
}
