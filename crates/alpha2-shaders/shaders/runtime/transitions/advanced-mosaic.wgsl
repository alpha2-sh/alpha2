// Port of gl-transitions/transitions/AdvancedMosaic.glsl
// Author: Sergey Kosarevsky
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let t = progress();
    let size = select(
        mix(params.pixel_size, 1.0, (t - 0.5) / 0.5),
        mix(1.0, params.pixel_size, t / 0.5),
        t < 0.5,
    );
    let d = size * 0.005;
    let cells = (uv - 0.5) / d;
    let coord = clamp(d * ceil(cells - 0.5) + 0.5, vec2f(0.0), vec2f(1.0));
    return mix(gl_from_color(coord), gl_to_color(coord), t);
}
