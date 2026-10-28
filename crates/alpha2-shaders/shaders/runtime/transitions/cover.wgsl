// The incoming picture slides in along `direction` over the outgoing one, which stays still.
fn original_direction(index: u32) -> vec2f {
    if index == 0u { return vec2f(-1.0, 0.0); }
    if index == 1u { return vec2f(1.0, 0.0); }
    if index == 2u { return vec2f(0.0, -1.0); }
    return vec2f(0.0, 1.0);
}

fn transition(uv: vec2f) -> vec4f {
    let d = original_direction(params.direction);
    // It arrives in place at progress 1.
    let q = uv + d * (1.0 - progress());
    let width = 1.0 / resolution();
    let inside = builtin_interval_coverage(q.x, width.x) * builtin_interval_coverage(q.y, width.y);
    return builtin_over(to_color(q) * inside, from_color(uv));
}
