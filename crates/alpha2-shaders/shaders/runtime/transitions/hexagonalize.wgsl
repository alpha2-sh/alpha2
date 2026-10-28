// Port of gl-transitions/transitions/hexagonalize.glsl
// Author: Fernando Kuteken
// License: MIT
// Hexagonal math from http://www.redblobgames.com/grids/hexagons/

// The nearest hexagon's axial coordinates (q, r) of a fractional (q, r).
fn round_hexagon(q: f32, r: f32) -> vec2f {
    let s = -q - r;
    var rounded_q = floor(q + 0.5);
    var rounded_r = floor(r + 0.5);
    let rounded_s = floor(s + 0.5);
    let delta_q = abs(rounded_q - q);
    let delta_r = abs(rounded_r - r);
    let delta_s = abs(rounded_s - s);
    if (delta_q > delta_r && delta_q > delta_s) {
        rounded_q = -rounded_r - rounded_s;
    } else if (delta_r > delta_s) {
        rounded_r = -rounded_q - rounded_s;
    }
    return vec2f(rounded_q, rounded_r);
}

fn gl_transition(uv: vec2f) -> vec4f {
    var dist = 2.0 * min(progress(), 1.0 - progress());
    if (params.steps > 0.0) {
        dist = ceil(dist * params.steps) / params.steps;
    }
    let size = 0.57735026 * dist / params.horizontal_hexagons;
    var point = uv;
    if (dist > 0.0) {
        let centered = (vec2f(uv.x, uv.y / ratio()) - 0.5) / size;
        let hexagon = round_hexagon(
            0.57735026 * centered.x + -0.33333334 * centered.y,
            0.6666667 * centered.y,
        );
        let x = (1.7320508 * hexagon.x + 0.8660254 * hexagon.y) * size + 0.5;
        let y = (1.5 * hexagon.y) * size + 0.5;
        point = vec2f(x, y * ratio());
    }
    return mix(gl_from_color(point), gl_to_color(point), progress());
}
