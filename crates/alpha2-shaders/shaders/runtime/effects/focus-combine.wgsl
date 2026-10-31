// Tilt-shift's last pass: sharp inside a band through (0.5, position), mixing in the `soft` and
// wider `blurred` copies with distance to approximate a growing blur.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let a = radians(params.angle);
    // The band runs along (cos a, -sin a), counter-clockwise on screen; this is its normal.
    let normal = vec2f(sin(a), cos(a));
    let off_focus = abs(dot((uv - vec2f(0.5, params.position)) * builtin_aspect(), normal));
    let w = builtin_smoothstep(params.focus * 0.5, params.focus * 0.5 + params.falloff, off_focus);
    let soft = sample_soft(uv);
    let blurred = sample_blurred(uv);
    return select(mix(soft, blurred, w * 2.0 - 1.0), mix(src, soft, w * 2.0), w < 0.5);
}
