// Port of gl-transitions/transitions/TilesWave.glsl
// Author: numb3r23
// License: MIT
// alpha2: the flipping tiles' edges are antialiased and the edge-on division is guarded.

// Upstream's squeeze of one axis of a tile toward its middle, `x` in 0..1 of the tile.
fn tiles_wave_squeeze(x: f32, sin_time: f32, divisor: f32) -> f32 {
    return select((x - 0.5) * 0.5 / divisor + 0.5, (x - sin_time) * 0.5 / divisor, x < 0.5);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let count = max(floor(params.tile_count), vec2f(1.0));
    let tile_size = 1.0 / count;
    let in_tile = fract(uv * count);
    let tile = floor(uv * count);
    let tiles = count.x * count.y;
    // Diagonal wave from bottom-left to top-right.
    let offset = (tile.y + tile.x * count.y) / tiles;
    let time_offset = clamp((progress() - offset) * tiles, 0.0, 0.5);
    let sin_time = 1.0 - abs(cos(fract(time_offset) * 3.14159265));
    let outgoing = sin_time <= 0.5;
    // The card spans |x - 0.5| <= |0.5 - sin_time| of its tile, on both sides of the flip.
    let half_card = abs(0.5 - sin_time);
    let footprint = fwidthFine(uv * count);
    let cover = clamp(vec2f(0.5) + (vec2f(half_card) - abs(in_tile - 0.5)) / max(footprint, vec2f(0.0000001)), vec2f(0.0), vec2f(1.0));
    let divisor = select(0.5 - sin_time, 0.0001, abs(0.5 - sin_time) < 0.0001);
    var squeezed = vec2f(tiles_wave_squeeze(in_tile.x, sin_time, divisor), tiles_wave_squeeze(in_tile.y, sin_time, divisor));
    squeezed = select(1.0 - squeezed, squeezed, outgoing);
    let flips = vec2<bool>(params.flip_x != 0u, params.flip_y != 0u);
    let card_point = select(in_tile, squeezed, flips);
    let coverage = select(vec2f(1.0), cover, flips);
    let global_uv = tile * tile_size + card_point * tile_size;
    let card = select(gl_to_color(global_uv), gl_from_color(global_uv), outgoing);
    let around = select(gl_to_color(uv), gl_from_color(uv), outgoing);
    return mix(around, card, coverage.x * coverage.y);
}
