// Port of gl-transitions/transitions/wind.glsl
// Author: gre
// License: MIT

// One random value in [0, 1) per output pixel row, from a PCG hash (Jarzynski and Olano,
// 2020) of the row index: the original `fract(sin(x) * ...)` drew per-GPU streaks.
fn wind_random(uv_y: f32) -> f32 {
    let row = u32(max(floor(uv_y * resolution().y), 0.0));
    let state = row * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    // The top 24 bits convert to f32 exactly.
    return f32(((word >> 22u) ^ word) >> 8u) / 16777216.0;
}

fn gl_transition(uv: vec2f) -> vec4f {
    let x = select(uv.x, 1.0 - uv.x, params.reversed != 0u);
    let random = wind_random(uv.y);
    // A descending ramp (size > 0): MSL, GLSL and SPIR-V `smoothstep` leave edge0 > edge1
    // undefined, so the helper defines it with WGSL's own arithmetic.
    let mask = builtin_smoothstep(
        0.0,
        -params.size,
        x * (1.0 - params.size) + params.size * random - progress() * (1.0 + params.size),
    );
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
