// Port of gl-transitions/transitions/splitSlideInHorizontal.glsl
// Author: OllyOllyOlly
// License: MIT
// alpha2: `reverse` is `reversed`, and the moving edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let modifier = select(1.0, -1.0, params.reversed != 0u);
    let to_x = select(
        (uv.x + (progress() * modifier)) - modifier,
        (uv.x - (progress() * modifier)) + modifier,
        uv.y > 0.5,
    );
    let to_p = vec2f(to_x, uv.y);
    let arrived = builtin_interval_coverage(to_p.x, 1.0 / resolution().x);
    return mix(gl_from_color(uv), gl_to_color(to_p), arrived);
}
