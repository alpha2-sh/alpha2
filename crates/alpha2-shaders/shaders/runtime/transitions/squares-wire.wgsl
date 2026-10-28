// Port of gl-transitions/transitions/squareswire.glsl
// Author: gre
// License: MIT
// alpha2: squares is rounded to whole counts and a zero direction falls back to the default;
// the squares' edges are antialiased against the unwrapped grid coordinate, and the front's
// ramp is at least a pixel wide (a smoothness of 0 is a hard edge upstream).
fn gl_transition(p: vec2f) -> vec4f {
    let grid = p * round(params.squares);
    let width = fwidthFine(grid);
    let direction = select(params.direction, vec2f(1.0, -0.5), length(params.direction) < 0.000001);
    var v = normalize(direction);
    v /= abs(v.x) + abs(v.y);
    let d = v.x * 0.5 + v.y * 0.5;
    let offset = params.smoothness;
    let front = v.x * p.x + v.y * p.y - (d - 0.5 + progress() * (1.0 + offset));
    let pr = builtin_aa_smoothstep(-offset, 0.0, front);
    // Each cell's square spans pr / 2 to 1 - pr / 2 of it: a pulse of duty 1 - pr.
    let gap = pr / 2.0;
    let a = builtin_aa_pulse(grid.x - gap, 1.0 - pr, width.x)
        * builtin_aa_pulse(grid.y - gap, 1.0 - pr, width.y);
    return mix(gl_from_color(p), gl_to_color(p), a);
}
