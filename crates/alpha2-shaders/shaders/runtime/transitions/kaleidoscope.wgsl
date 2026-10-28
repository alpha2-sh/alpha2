// Port of gl-transitions/transitions/kaleidoscope.glsl
// Author: nwoeanhinnogaehr
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    var p = uv - 0.5;
    var t = pow(progress(), params.power) * params.speed;
    for (var i = 0; i < 7; i++) {
        p = vec2f(sin(t) * p.x + cos(t) * p.y, sin(t) * p.y - cos(t) * p.x);
        t += params.angle;
        // GLSL's floor-mod, as builtin_floor_mod, inline: a helper call in a loop costs the
        // call budget.
        p = abs(p - 2.0 * floor(p / 2.0) - 1.0);
    }
    return mix(
        mix(gl_from_color(uv), gl_to_color(uv), progress()),
        mix(gl_from_color(p), gl_to_color(p), progress()),
        1.0 - 2.0 * abs(progress() - 0.5),
    );
}
