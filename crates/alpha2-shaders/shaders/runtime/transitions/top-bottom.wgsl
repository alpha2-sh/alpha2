// Port of gl-transitions/transitions/TopBottom.glsl
// Author: zhmy
// License: MIT
// alpha2: the opaque black is the `back_color` parameter, the receding picture dims towards
// it, and the frame edges are antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let size = mix(1.0, 3.0, progress() * 0.2);
    let spto = (uv + vec2f(-0.5, -0.5)) * vec2f(size, size) + vec2f(0.5, 0.5);
    let spfr = uv + vec2f(0.0, 1.0 - progress());
    let width = 1.0 / resolution();
    let arriving = builtin_interval_coverage(spfr.x, width.x) * builtin_interval_coverage(spfr.y, width.y);
    let receding = builtin_interval_coverage(spto.x, width.x * size)
        * builtin_interval_coverage(spto.y, width.y * size);
    let behind = mix(builtin_premultiply(params.back_color), gl_from_color(spto), receding * (1.0 - progress()));
    return mix(behind, gl_to_color(spfr), arriving);
}
