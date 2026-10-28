// A wipe along a procedural gradient: each pixel switches when progress passes its value.
// The conic pattern's seam (at `angle`) is antialiased like an edge when softness is 0. The
// noise pattern's values fill only part of 0..1, so it can finish a little before progress 1.

fn wipe_point(uv: vec2f) -> vec2f {
    return (uv - params.center) * vec2f(resolution().x / resolution().y, 1.0);
}

fn wipe_value(p: vec2f) -> f32 {
    let angle = params.angle * 3.14159265 / 180.0;
    if params.pattern == 0u { return dot(p, vec2f(cos(angle), -sin(angle))); }
    // Counter-clockwise on screen from `angle`.
    if params.pattern == 2u { return fract(atan2(-p.y, p.x) / 6.28318531 - params.angle / 360.0); }
    if params.pattern == 3u { return abs(p.x) + abs(p.y); }
    if params.pattern == 4u {
        let seed = u32(clamp(round(params.seed), 0.0, 16777215.0));
        let q = p / params.noise_scale;
        return builtin_value_noise(q, seed) * 0.6667
            + builtin_value_noise(q * 2.0 + vec2f(17.0, 5.0), seed) * 0.3333;
    }
    return length(p);
}

fn transition(uv: vec2f) -> vec4f {
    // Normalized by the frame's corners, so the wipe starts and ends exactly on the frame.
    let c0 = wipe_value(wipe_point(vec2f(0.0, 0.0)));
    let c1 = wipe_value(wipe_point(vec2f(1.0, 0.0)));
    let c2 = wipe_value(wipe_point(vec2f(0.0, 1.0)));
    let c3 = wipe_value(wipe_point(vec2f(1.0, 1.0)));
    // Radial and diamond start at the center; conic and noise span 0..1.
    let from_center = params.pattern == 1u || params.pattern == 3u;
    let unit_span = params.pattern == 2u || params.pattern == 4u;
    let low = select(select(min(min(c0, c1), min(c2, c3)), 0.0, from_center), 0.0, unit_span);
    let high = select(max(max(c0, c1), max(c2, c3)), 1.0, unit_span);
    let g = clamp((wipe_value(wipe_point(uv)) - low) / max(high - low, 0.000001), 0.0, 1.0);
    let value = select(g, 1.0 - g, params.reversed != 0u);
    let t = progress() * (1.0 + params.softness);
    let hard = builtin_aa_step(value, t);
    let soft = builtin_smoothstep(value, value + params.softness, t);
    return mix(from_color(uv), to_color(uv), select(soft, hard, params.softness == 0.0));
}
