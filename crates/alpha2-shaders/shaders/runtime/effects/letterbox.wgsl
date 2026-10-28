// Opaque bars that leave a window of `aspect` (width over height): above and below, or at the
// sides when the canvas is wider. Bars cover transparent pixels too.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let canvas = resolution().x / resolution().y;
    let window = select(vec2f(1.0, canvas / params.aspect), vec2f(params.aspect / canvas, 1.0), params.aspect < canvas);
    let low = (vec2f(1.0) - window) * 0.5;
    let p = (uv - low) / window;
    let width = fwidthFine(p);
    let inside = builtin_interval_coverage(p.x, width.x) * builtin_interval_coverage(p.y, width.y);
    return mix(builtin_premultiply(params.color), src, inside);
}
