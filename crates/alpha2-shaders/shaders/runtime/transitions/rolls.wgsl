// Port of gl-transitions/transitions/Rolls.glsl
// Author: Mark Craig
// License: MIT
// alpha2: `type` is the `corner` enum and `RotDown` is `rot_down`; the rolling frame's edges
// are antialiased.
const ROLLS_PI = 3.14159265358979323846;

fn gl_transition(uv: vec2f) -> vec4f {
    let resolution_uv = vec2f(ratio(), 1.0);
    let corner = params.corner;
    let down = params.rot_down != 0u;
    // Corners 0 and 1 turn one way, 2 and 3 the other; each mirrors uv so its pivot is (0, 0).
    let turn = select(select(-ROLLS_PI, ROLLS_PI, down), select(ROLLS_PI, -ROLLS_PI, down), corner >= 2u);
    let theta = turn / 2.0 * progress();
    let flip_x = corner == 0u || corner == 3u;
    let flip_y = corner == 2u || corner == 3u;
    let uvi = vec2f(select(uv.x, 1.0 - uv.x, flip_x), select(uv.y, 1.0 - uv.y, flip_y));
    let c1 = cos(theta);
    let s1 = sin(theta);
    let uv2 = vec2f(
        uvi.x * resolution_uv.x * c1 - uvi.y * resolution_uv.y * s1,
        uvi.x * resolution_uv.x * s1 + uvi.y * resolution_uv.y * c1,
    ) / resolution_uv;
    // The rotated frame's footprint per pixel, in its own uv.
    let width = (abs(c1) + abs(s1)) / resolution();
    let inside = builtin_interval_coverage(uv2.x, width.x) * builtin_interval_coverage(uv2.y, width.y);
    let rolled = vec2f(select(uv2.x, 1.0 - uv2.x, flip_x), select(uv2.y, 1.0 - uv2.y, flip_y));
    return mix(gl_to_color(uv), gl_from_color(rolled), inside);
}
