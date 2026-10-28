// Port of gl-transitions/transitions/burn0.glsl
// Author: liubailin2020@gmail.com
// License: MIT
// alpha2: replaces upstream's noise with `builtin_value_noise`, one seed per octave, and
// adds the burn colour as premultiplied light that also covers transparent pixels.
fn gl_transition(uv: vec2f) -> vec4f {
    let source = gl_from_color(uv);
    let destination = gl_to_color(uv);
    let p = uv * 4.0;
    let n = 0.5 * builtin_value_noise(p, 0u)
        + 0.25 * builtin_value_noise(p * 2.0, 1u)
        + 0.125 * builtin_value_noise(p * 4.0, 2u)
        + 0.0625 * builtin_value_noise(p * 8.0, 3u);
    let l = builtin_smoothstep(progress(), progress() + 0.05, n);
    let edge = (1.0 - l) * l * 5.0;
    let mixed = mix(destination, source, l);
    let glow = builtin_premultiply(params.burn_color) * edge;
    return builtin_valid(vec4f(mixed.rgb + glow.rgb, max(mixed.a, glow.a)));
}
