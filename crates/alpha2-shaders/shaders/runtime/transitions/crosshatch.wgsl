// Port of gl-transitions/transitions/crosshatch.glsl
// Author: pthrasher
// License: MIT
// alpha2: PCG hashes of the pixel row and column replace upstream's `fract(sin(...))`.
fn gl_transition(uv: vec2f) -> vec4f {
    let pixel = vec2u(max(floor(uv * resolution()), vec2f(0.0)));
    let row = builtin_unit(builtin_hash3(pixel.y, 0u, 0u));
    let column = builtin_unit(builtin_hash3(pixel.x, 1u, 0u));
    let dist = distance(params.center, uv) / params.threshold;
    let r = progress() - min(row, column);
    let fade_out = builtin_smoothstep(1.0 - params.fade_edge, 1.0, progress());
    let fade_in = builtin_smoothstep(0.0, params.fade_edge, progress());
    let amount = mix(0.0, mix(step(dist, r), 1.0, fade_out), fade_in);
    return mix(gl_from_color(uv), gl_to_color(uv), amount);
}
