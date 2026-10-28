// Port of gl-transitions/transitions/perlin.glsl
// Author: Rich Harris
// License: MIT
// alpha2: replaces upstream's noise and hash with `builtin_value_noise`, salted by the seed's
// bits, and antialiases the ramp where it is narrower than a pixel.
fn gl_transition(uv: vec2f) -> vec4f {
    let n = builtin_value_noise(uv * params.scale, bitcast<u32>(params.seed));
    let p = mix(-params.smoothness, 1.0 + params.smoothness, progress());
    let q = builtin_aa_smoothstep(p - params.smoothness, p + params.smoothness, n);
    return mix(gl_from_color(uv), gl_to_color(uv), 1.0 - q);
}
