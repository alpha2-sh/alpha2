// The bodies are ports of gl-transitions and keep its coordinates: uv (0, 0) is the
// bottom-left corner and y grows upward, so the entry point and the samplers flip y.
fn transition(uv: vec2f) -> vec4f {
    return gl_transition(vec2f(uv.x, 1.0 - uv.y));
}

fn gl_from_color(uv: vec2f) -> vec4f {
    return from_color(vec2f(uv.x, 1.0 - uv.y));
}

fn gl_to_color(uv: vec2f) -> vec4f {
    return to_color(vec2f(uv.x, 1.0 - uv.y));
}

// Built-in transitions store premultiplied, sRGB-encoded RGBA.
fn builtin_straight(color: vec4f) -> vec3f {
    if color.a <= 0.000001 { return vec3f(0.0); }
    return color.rgb / color.a;
}

fn builtin_premultiply(color: vec4f) -> vec4f {
    return vec4f(color.rgb * color.a, color.a);
}

// Explicitly handle hard edges and descending ramps on every backend.
fn builtin_smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 { return step(edge0, x); }
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn builtin_smoothstep4(edge0: vec4f, edge1: vec4f, x: f32) -> vec4f {
    return vec4f(
        builtin_smoothstep(edge0.r, edge1.r, x),
        builtin_smoothstep(edge0.g, edge1.g, x),
        builtin_smoothstep(edge0.b, edge1.b, x),
        builtin_smoothstep(edge0.a, edge1.a, x),
    );
}

// Pixel-sized coverage, separate from artistic feathering and temporal easing.
// Call before non-uniform branches: derivatives require uniform control flow.
// `fwidthFine`, not `fwidth`: GPUs choose fine or coarse (2x2-quad) differences for plain
// `fwidth`, which differ on non-linear fields. WebGL2 lowers this to `fwidth`.
fn builtin_aa_step(edge: f32, value: f32) -> f32 {
    let signed_distance = value - edge;
    let width = fwidthFine(signed_distance);
    // A uniform field has no spatial edge to antialias. Keep its step semantics.
    if width <= 0.0000001 { return step(edge, value); }
    return clamp(0.5 + signed_distance / width, 0.0, 1.0);
}

fn builtin_aa_smoothstep(edge0: f32, edge1: f32, value: f32) -> f32 {
    return builtin_aa_ramp(edge0, edge1, value, fwidthFine(value));
}

fn builtin_aa_ramp(edge0: f32, edge1: f32, value: f32, footprint: f32) -> f32 {
    let width = max(footprint, 0.0000001);
    let center = (edge0 + edge1) * 0.5;
    let half_span = max(abs(edge1 - edge0), width) * 0.5;
    let ramp = builtin_smoothstep(center - half_span, center + half_span, value);
    return select(ramp, 1.0 - ramp, edge1 < edge0);
}

// Integral of a repeating pulse. Antialias with the derivative of the unwrapped
// phase, so wrap seams are covered without smearing atan/fract discontinuities.
fn builtin_pulse_integral(phase: f32, duty: f32) -> f32 {
    return floor(phase) * duty + min(fract(phase), duty);
}

fn builtin_aa_pulse(phase: f32, duty: f32, width: f32) -> f32 {
    let span = max(width, 0.0000001);
    let coverage = (builtin_pulse_integral(phase + span * 0.5, duty)
        - builtin_pulse_integral(phase - span * 0.5, duty)) / span;
    return clamp(coverage, 0.0, 1.0);
}

fn builtin_angle_width(centered: vec2f) -> f32 {
    let squared_radius = max(dot(centered, centered), 0.0000001);
    return dot(abs(centered.yx), 1.0 / resolution()) / squared_radius;
}

fn builtin_interval_coverage(value: f32, width: f32) -> f32 {
    let span = max(width, 0.0000001);
    return clamp((min(value + span * 0.5, 1.0) - max(value - span * 0.5, 0.0)) / span, 0.0, 1.0);
}
