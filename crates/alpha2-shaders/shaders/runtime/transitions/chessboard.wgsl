// Port of gl-transitions/transitions/chessboard.glsl
// Author: lql
// License: MIT
// alpha2: the wipe inside each cell is antialiased; the checker's own borders stay hard.
fn gl_transition(uv: vec2f) -> vec4f {
    let st = uv * params.grid_num;
    // The unwrapped coordinate's footprint: fract's wrap at a cell border is no edge.
    let width = max(fwidthFine(st.x), 0.0000001);
    let idx = floor(st);
    let grid = fract(st);
    let checker = builtin_floor_mod(idx.x + idx.y, 2.0);
    let first = progress() <= 0.5;
    let t = select((progress() - 0.5) * 2.0, progress() * 2.0, first);
    // Upstream's step(grid.x, t), whole at t = 1 so the cell's far border stays covered.
    let wipe = select(clamp(0.5 + (t - grid.x) / width, 0.0, 1.0), 1.0, t >= 1.0);
    let amount = select(select(1.0, wipe, checker < 0.5), select(0.0, wipe, checker > 0.5), first);
    return mix(gl_from_color(uv), gl_to_color(uv), amount);
}
