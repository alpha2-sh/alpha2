// Integer arithmetic only, so grain is identical on every GPU (unlike a `sin`-based hash).

// PCG (Jarzynski and Olano, "Hash Functions for GPU Rendering", 2020).
fn film_grain_hash(value: u32) -> u32 {
    let state = value * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn effect(uv: vec2f, src: vec4f) -> vec4f {
    if src.a <= 0.0001 { return vec4f(0.0); }
    // Fixed-point division (1/256 pixel), since float division can land either side of a cell
    // edge.
    let pixel = vec2u(max(floor(uv * resolution()), vec2f(0.0)));
    let size = u32(round(max(params.size, 0.5) * 256.0));
    let cell = (pixel * 256u + vec2u(128u)) / size;
    // Seed with exact ticks; seconds lose resolution late on a long timeline.
    let ticks = time_ticks();
    let seed = film_grain_hash(ticks.x ^ film_grain_hash(ticks.y));
    let hash = film_grain_hash(cell.x ^ film_grain_hash(cell.y ^ seed));
    // The top 24 bits convert to f32 exactly.
    let noise = f32(hash >> 8u) / 16777216.0 - 0.5;
    let rgb = clamp(src.rgb / src.a + vec3f(noise * params.amount), vec3f(0.0), vec3f(1.0));
    return vec4f(rgb * src.a, src.a);
}
