// Port of gl-transitions/transitions/GridFlip.glsl
// Author: TimDonselaar
// License: MIT
// alpha2: a PCG hash of the tile replaces upstream's `fract(sin(...))`, the dividers and the
// card edges are antialiased, and the edge-on division is guarded.

// The distance from `p` to the nearest border of its tile.
fn grid_flip_delta(p: vec2f, size: vec2f) -> f32 {
    let position = floor(size * p);
    let tile = vec2f(1.0 / size.x, 1.0 / size.y);
    let top = tile.y * (position.y + 1.0);
    let bottom = tile.y * position.y;
    let left = tile.x * position.x;
    let right = tile.x * (position.x + 1.0);
    let min_x = min(abs(p.x - left), abs(p.x - right));
    let min_y = min(abs(p.y - top), abs(p.y - bottom));
    return min(min_x, min_y);
}

fn gl_transition(p: vec2f) -> vec4f {
    let size = max(floor(params.size), vec2f(1.0));
    let divider = min(1.0 / size.x, 1.0 / size.y) * params.divider_width;
    let bg = builtin_premultiply(params.bg_color);
    let position = floor(size * p);
    let r = builtin_unit(builtin_hash3(builtin_cell(position.x), builtin_cell(position.y), 0u)) - params.randomness;
    let flip = (progress() - params.pause) / (1.0 - params.pause * 2.0);
    let cp = builtin_smoothstep(0.0, 1.0 - r, flip);
    let tile = 1.0 / size.x;
    let delta = position.x * tile;
    let offset = tile / 2.0 + delta;
    let half_width = abs(cp - 0.5);
    let x = (p.x - offset) / max(half_width, 0.0001) * 0.5 + offset;
    let card = select(gl_to_color(vec2f(x, p.y)), gl_from_color(vec2f(x, p.y)), cp <= 0.5);
    // Every derivative before the phase branches: off the dividers, and on the card.
    let off_divider = select(1.0, builtin_aa_step(divider, grid_flip_delta(p, size)), divider > 0.0);
    let on_card = builtin_aa_step(abs(size.x * (p.x - delta) - 0.5), half_width);
    if progress() < params.pause {
        let fading = progress() / params.pause;
        return mix(bg, gl_from_color(p), mix(1.0 - fading, 1.0, off_divider));
    }
    if progress() < 1.0 - params.pause {
        return mix(bg, mix(bg, card, on_card), off_divider);
    }
    let fading = (progress() - 1.0 + params.pause) / params.pause;
    return mix(bg, gl_to_color(p), mix(fading, 1.0, off_divider));
}
