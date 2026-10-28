// Port of gl-transitions/transitions/splitSlideInOutHorizontal.glsl
// Author: OllyOllyOlly
// License: MIT
// alpha2: `reverse` is `reversed`, and the moving edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let modifier = select(-1.0, 1.0, uv.y > 0.5) * select(1.0, -1.0, params.reversed != 0u);
    let from_p = vec2f(uv.x + (progress() * modifier), uv.y);
    let to_p = vec2f((uv.x + (progress() * modifier)) - modifier, uv.y);
    let staying = builtin_interval_coverage(from_p.x, 1.0 / resolution().x);
    return mix(gl_to_color(to_p), gl_from_color(from_p), staying);
}
