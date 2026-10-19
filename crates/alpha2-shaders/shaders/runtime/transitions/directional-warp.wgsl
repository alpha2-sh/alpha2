// Port of gl-transitions/transitions/directionalwarp.glsl
// Author: pschroen
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    var direction = params.direction;
    if (length(direction) < 0.000001) {
        direction = vec2f(-1.0, 1.0);
    }
    var vector = normalize(direction);
    vector /= abs(vector.x) + abs(vector.y);
    let edge = dot(vector, vec2f(0.5));
    let mask = 1.0 - smoothstep(
        -params.smoothness,
        0.0,
        dot(vector, uv) - (edge - 0.5 + progress() * (1.0 + params.smoothness)),
    );
    let from_uv = (uv - vec2f(0.5)) * (1.0 - mask) + vec2f(0.5);
    let to_uv = (uv - vec2f(0.5)) * mask + vec2f(0.5);
    return mix(gl_from_color(from_uv), gl_to_color(to_uv), mask);
}
