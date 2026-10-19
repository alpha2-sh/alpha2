// Port of gl-transitions/transitions/directionalwipe.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    var direction = params.direction;
    if (length(direction) < 0.000001) {
        direction = vec2f(1.0, -1.0);
    }
    var vector = normalize(direction);
    vector /= abs(vector.x) + abs(vector.y);
    let edge = dot(vector, vec2f(0.5));
    let mask = (1.0 - step(progress(), 0.0)) * (1.0 - builtin_aa_smoothstep(
        -params.smoothness,
        0.0,
        dot(vector, uv) - (edge - 0.5 + progress() * (1.0 + params.smoothness)),
    ));
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
