// Port of gl-transitions/transitions/splitSlideInVertical.glsl
// Author: OllyOllyOlly
// License: MIT
// alpha2: `reverse` is `reversed`, and the moving edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let modifier = select(1.0, -1.0, params.reversed != 0u);
    let to_y = select(
        (uv.y + (progress() * modifier)) - modifier,
        (uv.y - (progress() * modifier)) + modifier,
        uv.x > 0.5,
    );
    let p = vec2f(uv.x, to_y);
    let arrived = builtin_interval_coverage(p.y, 1.0 / resolution().y);
    return mix(gl_from_color(uv), gl_to_color(p), arrived);
}
