// Port of gl-transitions/transitions/randomsquares.glsl
// Author: gre
// License: MIT
// alpha2: a PCG hash of the cell index replaces upstream's `fract(sin(...))`.
fn gl_transition(uv: vec2f) -> vec4f {
    // Upstream's size is an ivec2.
    let cells = max(floor(params.size), vec2f(1.0)) * uv;
    let threshold = builtin_unit(builtin_hash3(builtin_cell(cells.x), builtin_cell(cells.y), 0u));
    // A descending ramp, hard at smoothness 0.
    let mask = builtin_smoothstep(
        0.0,
        -params.smoothness,
        threshold - progress() * (1.0 + params.smoothness),
    );
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
