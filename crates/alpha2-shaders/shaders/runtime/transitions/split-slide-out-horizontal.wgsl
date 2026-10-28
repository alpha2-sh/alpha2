// Port of gl-transitions/transitions/splitSlideOutHorizontal.glsl
// Author: OllyOllyOlly
// License: MIT
// alpha2: `reverse` is `reversed`, and the moving edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let modifier = select(-1.0, 1.0, uv.y > 0.5) * select(1.0, -1.0, params.reversed != 0u);
    let p = vec2f(uv.x + (progress() * modifier), uv.y);
    let staying = builtin_interval_coverage(p.x, 1.0 / resolution().x);
    return mix(gl_to_color(uv), gl_from_color(p), staying);
}
