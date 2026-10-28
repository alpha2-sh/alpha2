// Port of gl-transitions/transitions/DirectionalScaled.glsl
// Author: Thibaut Foussard
// License: MIT
// alpha2: the frame borders are antialiased and the wrap seam is integrated as in
// `directional`; a (0, 0) direction takes the default one; sines through
// `directional_scaled_sin`, so the picture lands on the same subpixel on every GPU.
const DIRECTIONAL_SCALED_PI = 3.14159265358979;

// `sin(x)` reduced to a quarter turn and summed as its Taylor series to x^11 (within 1e-7):
// only multiplications and additions, so every GPU agrees, which drivers' `sin` does not.
fn directional_scaled_sin(x: f32) -> f32 {
    let turns = x * 0.159154943091895;
    let r = (turns - floor(turns)) * 6.28318530717959;
    let upper = r > 3.14159265358979;
    let h = select(r, r - 3.14159265358979, upper);
    let t = select(h, 3.14159265358979 - h, h > 1.57079632679490);
    let t2 = t * t;
    let s = t * (1.0 + t2 * (-0.166666666666667 + t2 * (0.00833333333333333
        + t2 * (-0.000198412698412698 + t2 * (0.00000275573192239859 - t2 * 0.0000000250521083854417)))));
    return select(s, -s, upper);
}

fn gl_transition(uv: vec2f) -> vec4f {
    var heading = sign(params.direction);
    if all(heading == vec2f(0.0)) { heading = vec2f(0.0, 1.0); }
    let wave = directional_scaled_sin(progress() * DIRECTIONAL_SCALED_PI / 2.0);
    let eased = wave * wave * wave;
    let s = 1.0 - (1.0 - (1.0 / params.scale)) * directional_scaled_sin(progress() * DIRECTIONAL_SCALED_PI);
    let width = 1.0 / resolution();
    let p = uv + eased * heading;
    // Off the seam one sample; on it the two images wrap, so integrate subpixel samples. The
    // distance to the seam along a moving axis decides, not the coverage, whose rounding flips
    // at interior pixels and at the frame's edge pixels.
    let to_seam = min(abs(p), abs(p - 1.0));
    var side = 1.0;
    if any((to_seam < width * 0.5) & (heading != vec2f(0.0))) { side = floor(sqrt(clamp(max_filter_samples(), 1.0, 16.0))); }
    // Each tap covers its share of the pixel.
    let border_span = max(width * s / side, vec2f(0.0000001));
    var color = vec4f(0.0);
    for (var y = 0u; y < u32(side); y++) {
        for (var x = 0u; x < u32(side); x++) {
            let offset = ((vec2f(f32(x), f32(y)) + 0.5) / side - 0.5) * width;
            let q = uv + offset + eased * heading;
            let f = (fract(q) - 0.5) * s + 0.5;
            let inside = step(0.0, q.y) * step(q.y, 1.0) * step(0.0, q.x) * step(q.x, 1.0);
            let flipped = vec2f(f.x, 1.0 - f.y);
            let tap = mix(to_color(flipped), from_color(flipped), inside);
            let border = clamp(
                (min(f + border_span * 0.5, vec2f(1.0)) - max(f - border_span * 0.5, vec2f(0.0))) / border_span,
                vec2f(0.0),
                vec2f(1.0),
            );
            color += tap * (border.x * border.y);
        }
    }
    return color / (side * side);
}
