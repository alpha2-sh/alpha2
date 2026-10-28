// Shared by the built-in effect bodies, which are appended after it.
const BUILTIN_PI: f32 = 3.14159265;
const BUILTIN_TAU: f32 = 6.28318531;
const BUILTIN_LUMA: vec3f = vec3f(0.2126, 0.7152, 0.0722);

fn builtin_straight(c: vec4f) -> vec3f {
    if c.a <= 0.0001 { return vec3f(0.0); }
    return c.rgb / c.a;
}

// A straight color and an alpha as the premultiplied value an effect returns, both in 0..1.
fn builtin_output(rgb: vec3f, alpha: f32) -> vec4f {
    let a = clamp(alpha, 0.0, 1.0);
    return vec4f(clamp(rgb, vec3f(0.0), vec3f(1.0)) * a, a);
}

fn builtin_premultiply(color: vec4f) -> vec4f { return vec4f(color.rgb * color.a, color.a); }

fn builtin_decode(encoded: f32) -> f32 {
    if encoded <= 0.04045 { return encoded / 12.92; }
    return pow((encoded + 0.055) / 1.055, 2.4);
}

fn builtin_encode(linear: f32) -> f32 {
    if linear <= 0.0031308 { return linear * 12.92; }
    return 1.055 * pow(linear, 1.0 / 2.4) - 0.055;
}

fn builtin_decode3(rgb: vec3f) -> vec3f {
    return vec3f(builtin_decode(rgb.r), builtin_decode(rgb.g), builtin_decode(rgb.b));
}

fn builtin_encode3(rgb: vec3f) -> vec3f {
    return vec3f(builtin_encode(rgb.r), builtin_encode(rgb.g), builtin_encode(rgb.b));
}

fn builtin_luminance(rgb: vec3f) -> f32 { return dot(rgb, BUILTIN_LUMA); }

// Gray of an encoded color: its BT.709 luminance in linear light, encoded again.
fn builtin_gray(rgb: vec3f) -> f32 {
    return builtin_encode(clamp(builtin_luminance(builtin_decode3(rgb)), 0.0, 1.0));
}

// Hard edges and descending ramps defined on every backend.
fn builtin_smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 { return step(edge0, x); }
    let t = clamp((x - edge0) / (edge1 - edge0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn builtin_interval_coverage(value: f32, width: f32) -> f32 {
    let span = max(width, 0.0000001);
    return clamp((min(value + span * 0.5, 1.0) - max(value - span * 0.5, 0.0)) / span, 0.0, 1.0);
}

// PCG (Jarzynski and Olano 2020), as film-grain and wind use it.
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

fn builtin_seed(seed: f32) -> u32 { return u32(clamp(round(seed), 0.0, 16777215.0)); }

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

// Positions in output heights about `center`, so distances are round on any aspect.
fn builtin_aspect() -> vec2f { return vec2f(resolution().x / resolution().y, 1.0); }

fn builtin_rotate(v: vec2f, radians: f32) -> vec2f {
    let c = cos(radians);
    let s = sin(radians);
    return vec2f(c * v.x - s * v.y, s * v.x + c * v.y);
}

// Edge modes: 0 transparent, 1 clamp, 2 repeat, 3 mirror.
fn builtin_wrap(uv: vec2f, mode: u32) -> vec2f {
    if mode == 2u { return fract(uv); }
    if mode == 3u { return vec2f(1.0) - abs(vec2f(1.0) - 2.0 * fract(uv * 0.5)); }
    return uv;
}

// How much of the pixel whose source position is `uv` lies inside the source, `width`
// being fwidthFine(uv) taken by the caller in uniform control flow.
fn builtin_edge_coverage(uv: vec2f, width: vec2f, mode: u32) -> f32 {
    if mode != 0u { return 1.0; }
    return builtin_interval_coverage(uv.x, width.x) * builtin_interval_coverage(uv.y, width.y);
}

fn builtin_sample_edge(uv: vec2f, width: vec2f, mode: u32) -> vec4f {
    return sample_src(builtin_wrap(uv, mode)) * builtin_edge_coverage(uv, width, mode);
}
