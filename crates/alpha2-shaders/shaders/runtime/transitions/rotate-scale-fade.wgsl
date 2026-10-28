// Port of gl-transitions/transitions/rotate_scale_fade.glsl
// Author: Fernando Kuteken
// License: MIT
// alpha2: turns in a square space, so the picture keeps its shape on any frame, and the frame's
// edge against `back_color` is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let aspect = vec2f(ratio(), 1.0);
    let difference = (uv - params.center) * aspect;
    // Whole turns dropped first: the same angle, a small argument.
    let angle = 6.28318531 * fract(params.rotations * progress());
    let c = cos(angle);
    let s = sin(angle);
    let current_scale = mix(params.scale, 1.0, 2.0 * abs(progress() - 0.5));
    let turned = vec2f(difference.x * c - difference.y * s, difference.x * s + difference.y * c);
    let q = params.center + turned / current_scale / aspect;
    let width = fwidthFine(q);
    let inside = builtin_interval_coverage(q.x, width.x) * builtin_interval_coverage(q.y, width.y);
    let image = mix(gl_from_color(q), gl_to_color(q), progress());
    return mix(builtin_premultiply(params.back_color), image, inside);
}
