//! WGSL for paints, shared by the generator pass and the vector paint pass,
//! so a gradient is the same color wherever it is drawn.

use crate::generator::MAX_GENERATOR_STOPS;

/// `AlphaPaint` and the functions that color a point with it.
///
/// `builtin_paint(paint, uv, res)` is the premultiplied linear-light color
/// of `paint` at `uv`, normalized in a picture of `res` pixels. Stops are
/// straight sRGB as authored and interpolate premultiplied in linear light.
/// `builtin_generator_store` encodes such a color as the engine stores
/// pictures: premultiplied, sRGB-encoded.
#[must_use]
pub(crate) fn paint_wgsl() -> String {
    format!(
        r"struct AlphaPaint {{
    kind: u32,
    a: vec2f,
    b: vec2f,
    spread: u32,
    count: u32,
    offsets: array<f32, {MAX_GENERATOR_STOPS}>,
    // Straight sRGB, as authored.
    colors: array<vec4f, {MAX_GENERATOR_STOPS}>,
}};

// Straight sRGB to premultiplied linear light; 1 stays exactly 1.
fn builtin_stop_color(color: vec4f) -> vec4f {{
    let c = clamp(color.rgb, vec3f(0.0), vec3f(1.0));
    let curve = pow((c + vec3f(0.055)) / 1.055, vec3f(2.4));
    let linear = select(select(curve, c / 12.92, c <= vec3f(0.04045)), vec3f(1.0), c >= vec3f(1.0));
    let alpha = clamp(color.a, 0.0, 1.0);
    return vec4f(linear * alpha, alpha);
}}

// Where `uv` lies along the paint's gradient, before spread, in a picture of `res` pixels.
fn builtin_paint_position(paint: AlphaPaint, uv: vec2f, res: vec2f) -> f32 {{
    let p = uv * res;
    if (paint.kind == 2u) {{
        let start = paint.a * res;
        let direction = paint.b * res - start;
        return dot(p - start, direction) / max(dot(direction, direction), 0.000001);
    }}
    if (paint.kind == 3u) {{
        return length(p - paint.a * res) / max(paint.b.x * min(res.x, res.y), 0.000001);
    }}
    // Conic: clockwise from straight up (-y), a whole turn from 0 to 1.
    let offset = p - paint.a * res;
    if (dot(offset, offset) <= 0.0) {{ return 0.0; }}
    let angle = atan2(offset.x, -offset.y) - paint.b.x;
    let turn = angle / 6.283185307179586;
    return turn - floor(turn);
}}

fn builtin_paint_spread(paint: AlphaPaint, t: f32) -> f32 {{
    if (paint.kind == 4u) {{ return t; }}
    if (paint.spread == 1u) {{ return t - floor(t); }}
    if (paint.spread == 2u) {{
        let m = t - 2.0 * floor(t * 0.5);
        return select(m, 2.0 - m, m > 1.0);
    }}
    return clamp(t, 0.0, 1.0);
}}

// Premultiplied linear-light color of the paint at `uv`, in a picture of `res` pixels.
fn builtin_paint(paint: AlphaPaint, uv: vec2f, res: vec2f) -> vec4f {{
    if (paint.kind == 0u || paint.count == 0u) {{ return vec4f(0.0); }}
    var colors = paint.colors;
    var offsets = paint.offsets;
    if (paint.kind == 1u || paint.count == 1u) {{ return builtin_stop_color(colors[0]); }}
    let t = builtin_paint_spread(paint, builtin_paint_position(paint, uv, res));
    if (t <= offsets[0]) {{ return builtin_stop_color(colors[0]); }}
    for (var stop = 1u; stop < {MAX_GENERATOR_STOPS}u; stop++) {{
        if (stop >= paint.count) {{ break; }}
        if (t <= offsets[stop]) {{
            let span = offsets[stop] - offsets[stop - 1u];
            let f = select((t - offsets[stop - 1u]) / span, 1.0, span <= 0.0);
            return mix(builtin_stop_color(colors[stop - 1u]), builtin_stop_color(colors[stop]), f);
        }}
    }}
    return builtin_stop_color(colors[paint.count - 1u]);
}}

// Premultiplied linear light to the premultiplied, sRGB-encoded storage
// value; 1 stays exactly 1.
fn builtin_generator_store(color: vec4f) -> vec4f {{
    if (color.a <= 0.0) {{ return vec4f(0.0); }}
    let c = clamp(color.rgb / color.a, vec3f(0.0), vec3f(1.0));
    let curve = 1.055 * pow(c, vec3f(1.0 / 2.4)) - vec3f(0.055);
    let encoded = select(select(curve, c * 12.92, c <= vec3f(0.0031308)), vec3f(1.0), c >= vec3f(1.0));
    return vec4f(encoded * color.a, color.a);
}}
"
    )
}
