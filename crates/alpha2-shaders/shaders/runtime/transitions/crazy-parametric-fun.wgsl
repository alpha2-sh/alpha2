// Port of gl-transitions/transitions/CrazyParametricFun.glsl
// Author: mandubian
// License: MIT
// alpha2: sines and cosines through `crazy_parametric_sin`: upstream's arguments reach
// hundreds of radians.
// `sin(x)` reduced to a quarter turn and summed as its Taylor series to x^11 (within 1e-7):
// only multiplications and additions, so every GPU agrees, which drivers' `sin` does not.
fn crazy_parametric_sin(x: f32) -> f32 {
    let turns = x * 0.159154943091895;
    let r = (turns - floor(turns)) * 6.28318530717959;
    let upper = r > 3.14159265358979;
    let h = select(r, r - 3.14159265358979, upper);
    let t = select(h, 3.14159265358979 - h, h > 1.57079632679490);
    let t2 = t * t;
    let s = t * (1.0 + t2 * (-0.166666666666667 + t2 * (0.00833333333333333
        + t2 * (-0.000198412698412698 + t2 * (0.00000275573192239859 - t2 * 0.0000000250521083854417)))));
    return select(s, -s, upper);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let dir = uv - vec2f(0.5);
    let dist = length(dir);
    let x = (params.a - params.b) * crazy_parametric_sin(progress() + 1.5707963267949)
        + params.b * crazy_parametric_sin(progress() * ((params.a / params.b) - 1.0) + 1.5707963267949);
    let y = (params.a - params.b) * crazy_parametric_sin(progress())
        - params.b * crazy_parametric_sin(progress() * ((params.a / params.b) - 1.0));
    let offset = dir * vec2f(
        crazy_parametric_sin(progress() * dist * params.amplitude * x),
        crazy_parametric_sin(progress() * dist * params.amplitude * y),
    ) / params.smoothness;
    return mix(
        gl_from_color(uv + offset),
        gl_to_color(uv),
        builtin_smoothstep(0.2, 1.0, progress()),
    );
}
