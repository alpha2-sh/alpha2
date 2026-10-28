// Port of gl-transitions/transitions/RotateScaleVanish.glsl
// Author: Mark Craig
// License: MIT
// alpha2: `back_color` replaces upstream's opaque black, and the shrinking frame's edge is
// antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let aspect = vec2f(ratio(), 1.0);
    let reversed = params.reverse_effect != 0u;
    let t = select(progress(), 1.0 - progress(), reversed);
    let theta = select(-6.28318531 * t, 6.28318531 * t, params.reverse_rotation != 0u);
    let c1 = cos(theta);
    let s1 = sin(theta);
    let rad = max(0.00001, 1.0 - t);
    let centred = (uv - 0.5) * aspect;
    let turned = vec2f(centred.x * c1 - centred.y * s1, centred.x * s1 + centred.y * c1) / rad;
    let q = (turned + aspect / 2.0) / aspect;
    let width = fwidthFine(q);
    let inside = builtin_interval_coverage(q.x, width.x) * builtin_interval_coverage(q.y, width.y);
    let color_to = select(gl_to_color(uv), gl_from_color(uv), reversed);
    let shrinking = select(gl_from_color(q), gl_to_color(q), reversed);
    let outside = select(color_to, builtin_premultiply(params.back_color), params.fade_in_second != 0u);
    let col3 = mix(outside, shrinking, inside);
    return (1.0 - t) * col3 + t * color_to;
}
