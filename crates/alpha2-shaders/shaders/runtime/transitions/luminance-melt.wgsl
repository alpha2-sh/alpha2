// Port of gl-transitions/transitions/luminance_melt.glsl
// Author: 0gust1
// License: MIT
// alpha2: builtin_value_noise, mapped to -1..1, replaces upstream's simplex noise and a PCG
// hash of the pixel column its `fract(sin(...))` column noise; luminance is of straight color.
fn gl_transition(uv: vec2f) -> vec4f {
    let center = vec2f(1.0, f32(params.direction));
    let noise = 2.0 * builtin_value_noise(vec2f(uv.x, 0.0), 0u) - 1.0;
    let dist = distance(center, uv) - progress() * exp(noise);
    let column = builtin_unit(builtin_hash(builtin_cell(uv.x * resolution().x)));
    let r = progress() - column;
    let source = gl_from_color(uv);
    let straight = builtin_straight(source);
    let luminance = straight.r * 0.299 + straight.g * 0.587 + straight.b * 0.114;
    let below = luminance < params.l_threshold;
    let bright = select(below, luminance > params.l_threshold, params.above != 0u);
    let m = select(progress() * progress() * progress(), 1.0, dist <= r && bright);
    return mix(source, gl_to_color(uv), m);
}
