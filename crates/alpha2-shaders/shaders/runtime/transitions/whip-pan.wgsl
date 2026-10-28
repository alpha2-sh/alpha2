// Both pictures pushed along `direction`, blurred along it, most at the middle.
fn whip_direction(index: u32) -> vec2f {
    if index == 0u { return vec2f(-1.0, 0.0); }
    if index == 1u { return vec2f(1.0, 0.0); }
    if index == 2u { return vec2f(0.0, -1.0); }
    return vec2f(0.0, 1.0);
}

fn transition(uv: vec2f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 1.0, 64.0));
    let d = whip_direction(params.direction);
    let axis = abs(d);
    let texel = dot(axis, 1.0 / resolution());
    // Never under a pixel, so the seam between the pictures stays smooth.
    let streak = max(params.blur * 0.5 * sin(3.14159265 * progress()), texel);
    // The incoming picture follows the outgoing one in; ahead of the outgoing one its own edge
    // repeats.
    let forward = dot(d, axis) > 0.0;
    var sum = vec4f(0.0);
    for (var i = 0u; i < taps; i++) {
        let q = uv - d * progress() + d * (streak * ((f32(i) + 0.5) / f32(taps) - 0.5));
        let along = dot(q, axis);
        let incoming = select(along > 1.0, along < 0.0, forward);
        sum += select(from_color(q), to_color(q + d), incoming);
    }
    return sum / f32(taps);
}
