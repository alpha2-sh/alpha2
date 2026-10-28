// Port of gl-transitions/transitions/BlockDissolve.glsl
// Author: nwoeanhinnogaehr
// License: MIT
// alpha2: a PCG hash of the block index replaces upstream's `fract(sin(...))`.
fn gl_transition(uv: vec2f) -> vec4f {
    let block = uv / params.block_size;
    let threshold = builtin_unit(builtin_hash3(builtin_cell(block.x), builtin_cell(block.y), 0u));
    return mix(gl_from_color(uv), gl_to_color(uv), step(threshold, progress()));
}
