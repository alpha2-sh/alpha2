// Port of gl-transitions/transitions/pixelize.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let distance_to_end = min(progress(), 1.0 - progress());
    var distance_step = distance_to_end;
    if (params.steps > 0.0) {
        distance_step = ceil(distance_to_end * params.steps) / params.steps;
    }
    var sample_uv = uv;
    if (distance_step > 0.0) {
        let square_size = 2.0 * distance_step / max(params.squares_min, vec2f(1.0));
        sample_uv = (floor(uv / square_size) + vec2f(0.5)) * square_size;
    }
    return mix(gl_from_color(sample_uv), gl_to_color(sample_uv), progress());
}
