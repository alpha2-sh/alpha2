// Rows of the picture jump sideways with their color channels split, strongest mid-way;
// each row switches from the outgoing picture to the incoming one at its own moment.

fn transition(uv: vec2f) -> vec4f {
    let seed = u32(clamp(round(params.seed), 0.0, 16777215.0));
    let strength = sin(3.14159265 * progress()) * params.amount;
    let row = builtin_cell(uv.y / params.block_size);
    // New offsets 24 times over the transition.
    let stage = u32(floor(progress() * 24.0));
    let h = builtin_hash3(row, stage, seed);
    let jumping = select(0.0, 1.0, builtin_unit(builtin_hash(h)) < strength);
    let shift = vec2f((builtin_unit(h) - 0.5) * 0.25 * strength * jumping, 0.0);
    let split = vec2f(0.015 * strength * jumping, 0.0);
    let switched = builtin_unit(builtin_hash3(row, 4096u, seed)) < progress();
    let p = uv + shift;
    let base = select(from_color(p), to_color(p), switched);
    let red = select(from_color(p + split), to_color(p + split), switched);
    let blue = select(from_color(p - split), to_color(p - split), switched);
    return vec4f(red.r, base.g, blue.b, max(base.a, max(red.a, blue.a)));
}
