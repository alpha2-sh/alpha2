// Port of gl-transitions/transitions/TVStatic.glsl
// Author: Brandon Anzaldi
// License: MIT
// alpha2: a PCG hash of the pixel and of the progress replaces upstream's `fract(sin(...))`.
// The static is opaque, as upstream.

fn gl_transition(p: vec2f) -> vec4f {
    let pixel = floor(p * resolution());
    let noise = builtin_unit(builtin_hash3(builtin_cell(pixel.x), builtin_cell(pixel.y), bitcast<u32>(progress())));
    let noise_color = vec4f(vec3f(noise), 1.0);
    let shown = select(noise_color, gl_to_color(p), progress() > 1.0 - params.offset);
    return select(shown, gl_from_color(p), progress() < params.offset);
}
