// The outgoing picture slides away along `direction`, revealing the incoming one in place.
fn original_direction(index: u32) -> vec2f {
    if index == 0u { return vec2f(-1.0, 0.0); }
    if index == 1u { return vec2f(1.0, 0.0); }
    if index == 2u { return vec2f(0.0, -1.0); }
    return vec2f(0.0, 1.0);
}

fn transition(uv: vec2f) -> vec4f {
    let d = original_direction(params.direction);
    let q = uv - d * progress();
    let width = 1.0 / resolution();
    let inside = builtin_interval_coverage(q.x, width.x) * builtin_interval_coverage(q.y, width.y);
    return builtin_over(from_color(q) * inside, to_color(uv));
}
