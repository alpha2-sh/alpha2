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

// PCG (Jarzynski and Olano, "Hash Functions for GPU Rendering", 2020): integer hashes that
// are the same on every GPU, unlike `fract(sin(x) * ...)`.
fn builtin_hash(value: u32) -> u32 {
    let state = value * 747796405u + 2891336453u;
    let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
    return (word >> 22u) ^ word;
}

fn builtin_hash3(a: u32, b: u32, c: u32) -> u32 {
    return builtin_hash(a ^ builtin_hash(b ^ builtin_hash(c)));
}

// The top 24 bits of a hash as a float in [0, 1), converted exactly.
fn builtin_unit(hash: u32) -> f32 { return f32(hash >> 8u) / 16777216.0; }

// A lattice cell index of either sign as a hash input.
fn builtin_cell(value: f32) -> u32 { return bitcast<u32>(i32(floor(value))); }

// GLSL's floor-mod; WGSL `%` truncates toward zero.
fn builtin_floor_mod(x: f32, y: f32) -> f32 { return x - y * floor(x / y); }

// Value noise in [0, 1] on the integer lattice, smooth (C1) between lattice points.
fn builtin_value_noise(p: vec2f, seed: u32) -> f32 {
    let cell = floor(p);
    let f = p - cell;
    let w = f * f * (3.0 - 2.0 * f);
    let x = bitcast<u32>(i32(cell.x));
    let y = bitcast<u32>(i32(cell.y));
    let a = builtin_unit(builtin_hash3(x, y, seed));
    let b = builtin_unit(builtin_hash3(x + 1u, y, seed));
    let c = builtin_unit(builtin_hash3(x, y + 1u, seed));
    let d = builtin_unit(builtin_hash3(x + 1u, y + 1u, seed));
    return mix(mix(a, b, w.x), mix(c, d, w.x), w.y);
}

// `top` over `bottom`, both premultiplied.
fn builtin_over(top: vec4f, bottom: vec4f) -> vec4f { return top + bottom * (1.0 - top.a); }

// Clamped into premultiplied range: alpha 0..1, color 0..alpha.
fn builtin_valid(c: vec4f) -> vec4f {
    let a = clamp(c.a, 0.0, 1.0);
    return vec4f(clamp(c.rgb, vec3f(0.0), vec3f(a)), a);
}

// Hue, saturation, value (Smith 1978), hue in [0, 1).
fn builtin_rgb_to_hsv(c: vec3f) -> vec3f {
    let high = max(c.r, max(c.g, c.b));
    let low = min(c.r, min(c.g, c.b));
    let range = high - low;
    var hue = 0.0;
    if range > 0.0 {
        if high == c.r {
            hue = (c.g - c.b) / range;
            if hue < 0.0 { hue += 6.0; }
        } else if high == c.g {
            hue = (c.b - c.r) / range + 2.0;
        } else {
            hue = (c.r - c.g) / range + 4.0;
        }
    }
    return vec3f(hue / 6.0, select(0.0, range / high, high > 0.0), high);
}

fn builtin_hsv_to_rgb(hsv: vec3f) -> vec3f {
    let h = fract(hsv.x) * 6.0;
    let k = (vec3f(5.0, 3.0, 1.0) + vec3f(h)) % vec3f(6.0);
    let ramp = clamp(min(k, vec3f(4.0) - k), vec3f(0.0), vec3f(1.0));
    return vec3f(hsv.z) - hsv.z * hsv.y * ramp;
}
