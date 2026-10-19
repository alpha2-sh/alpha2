// Port of gl-transitions/transitions/squeeze.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let y = 0.5 + (uv.y - 0.5) / (1.0 - progress());
    let coverage = builtin_interval_coverage(y, fwidthFine(y));
    if (coverage <= 0.0) {
        return gl_to_color(uv);
    }
    let sample_uv = vec2f(uv.x, y);
    let offset = progress() * vec2f(0.0, params.color_separation);
    let center = gl_from_color(sample_uv);
    let negative = gl_from_color(sample_uv - offset);
    let positive = gl_from_color(sample_uv + offset);
    // Cover all three displaced channels, including fringes outside the center
    // sample. Their RGB values are already associated with alpha.
    let alpha = max(center.a, max(negative.a, positive.a));
    return mix(gl_to_color(uv), vec4f(negative.r, center.g, positive.b, alpha), coverage);
}
