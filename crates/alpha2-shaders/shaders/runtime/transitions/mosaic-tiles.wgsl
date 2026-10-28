// Port of gl-transitions/transitions/Mosaic.glsl
// Author: Xaychru
// License: MIT
// alpha2: a PCG hash of the tile replaces upstream's sine hash, and quarter turns are exact.
fn gl_transition(uv: vec2f) -> vec4f {
    let pi = 3.14159265358979323;
    let p = uv - 0.5;
    let rpr = progress() * 2.0 - 1.0;
    let az = abs(-(rpr * rpr * 2.0) + 3.0);
    let goal = floor(vec2f(params.end_x, params.end_y) + 0.5);
    let ease = -cos(progress() * pi) / 2.0 + 0.5;
    let rp = p * az + mix(vec2f(0.5), goal + 0.5, ease * ease);
    let tile = floor(rp);
    let m = rp - tile;
    let on_end = all(tile == goal);
    // One value turns the tile and picks its picture, as upstream.
    let r = builtin_unit(builtin_hash3(builtin_cell(rp.x), builtin_cell(rp.y), 0u));
    let quarter = select(u32(r * 4.0), 0u, on_end);
    let v = m - 0.5;
    var turned = v;
    if quarter == 1u { turned = vec2f(v.y, -v.x); }
    if quarter == 2u { turned = -v; }
    if quarter == 3u { turned = vec2f(-v.y, v.x); }
    let q = vec2f(0.5) + turned;
    return select(gl_from_color(q), gl_to_color(q), on_end || r > 0.5);
}
