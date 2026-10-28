// Port of gl-transitions/transitions/DefocusBlur.glsl
// Author: Sergey Kosarevsky
// License: MIT
// alpha2: a Vogel disc of max_filter_samples() taps replaces upstream's 13-tap Poisson table,
// and the disc is round on any frame: its radius is a fraction of the height.
fn gl_transition(uv: vec2f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 1.0, 64.0));
    let t = progress();
    let d = select(mix(params.blur_size, 0.0, (t - 0.5) / 0.5), mix(0.0, params.blur_size, t / 0.5), t < 0.5);
    let reach = vec2f(d / ratio(), d);
    var c0 = vec4f(0.0);
    var c1 = vec4f(0.0);
    for (var i = 0u; i < taps; i++) {
        // Equal-area rings turned by the golden angle, the angle reduced to one turn first.
        let r = sqrt((f32(i) + 0.5) / f32(taps));
        let a = fract(f32(i) * 0.381966011) * 6.28318531;
        let p = uv + vec2f(cos(a) * r, sin(a) * r) * reach;
        c0 += from_color(vec2f(p.x, 1.0 - p.y));
        c1 += to_color(vec2f(p.x, 1.0 - p.y));
    }
    return mix(c0 / f32(taps), c1 / f32(taps), t);
}
