// Rows of blocks jump sideways with their red and blue split, a new pattern `speed` times a
// second; at speed zero the pattern holds still.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let frame = u32(max(floor(time() * params.speed), 0.0));
    let row = u32(max(floor(uv.y / params.block_size), 0.0));
    let h = builtin_hash3(row, frame, builtin_seed(params.seed));
    let jumping = select(0.0, 1.0, builtin_unit(h) < params.amount * 0.5);
    let shift = (builtin_unit(builtin_hash(h)) - 0.5) * 0.2 * params.amount * jumping;
    let p = vec2f(uv.x + shift, uv.y);
    let split = vec2f(params.color_split * jumping / builtin_aspect().x, 0.0);
    let base = sample_src(p);
    let red = sample_src(p + split);
    let blue = sample_src(p - split);
    return vec4f(red.r, base.g, blue.b, max(base.a, max(red.a, blue.a)));
}
