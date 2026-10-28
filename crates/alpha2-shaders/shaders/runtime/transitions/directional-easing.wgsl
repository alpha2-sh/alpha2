// Port of gl-transitions/transitions/directional-easing.glsl
// Author: Max Plotnikov
// License: MIT
// alpha2: the wrap seam is integrated as in `directional`; a (0, 0) direction takes the
// default one.
fn gl_transition(uv: vec2f) -> vec4f {
    var heading = sign(params.direction);
    if all(heading == vec2f(0.0)) { heading = vec2f(0.0, 1.0); }
    let easing = sqrt((2.0 - progress()) * progress());
    let width = 1.0 / resolution();
    let p = uv + easing * heading;
    // Off the seam one sample; on it the two images wrap, so integrate subpixel samples. The
    // distance to the seam along a moving axis decides, not the coverage, whose rounding flips
    // at interior pixels and at the frame's edge pixels.
    let to_seam = min(abs(p), abs(p - 1.0));
    var side = 1.0;
    if any((to_seam < width * 0.5) & (heading != vec2f(0.0))) { side = floor(sqrt(clamp(max_filter_samples(), 1.0, 16.0))); }
    var color = vec4f(0.0);
    for (var y = 0u; y < u32(side); y++) {
        for (var x = 0u; x < u32(side); x++) {
            let offset = ((vec2f(f32(x), f32(y)) + 0.5) / side - 0.5) * width;
            let q = uv + offset + easing * heading;
            let f = fract(q);
            let inside = step(0.0, q.y) * step(q.y, 1.0) * step(0.0, q.x) * step(q.x, 1.0);
            let flipped = vec2f(f.x, 1.0 - f.y);
            color += mix(to_color(flipped), from_color(flipped), inside);
        }
    }
    return color / (side * side);
}
