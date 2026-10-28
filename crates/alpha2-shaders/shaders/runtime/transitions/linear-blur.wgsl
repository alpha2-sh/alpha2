// Port of gl-transitions/transitions/LinearBlur.glsl
// Author: gre
// License: MIT
// alpha2: a square grid of max_filter_samples() taps per picture, centred on the pixel, and the
// box is square on any frame: its side is a fraction of the height.
fn gl_transition(uv: vec2f) -> vec4f {
    let side = floor(sqrt(clamp(max_filter_samples(), 1.0, 64.0)));
    let disp = params.intensity * (0.5 - abs(0.5 - progress()));
    let reach = vec2f(disp / ratio(), disp);
    var c1 = vec4f(0.0);
    var c2 = vec4f(0.0);
    for (var xi = 0u; xi < u32(side); xi++) {
        for (var yi = 0u; yi < u32(side); yi++) {
            let v = (vec2f(f32(xi), f32(yi)) + 0.5) / side - 0.5;
            let p = uv + v * reach;
            c1 += from_color(vec2f(p.x, 1.0 - p.y));
            c2 += to_color(vec2f(p.x, 1.0 - p.y));
        }
    }
    let count = side * side;
    return mix(c1 / count, c2 / count, progress());
}
