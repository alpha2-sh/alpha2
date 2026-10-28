// Port of gl-transitions/transitions/burn.glsl
// Author: gre
// License: MIT
// alpha2: the colour is added as premultiplied light where each picture is, not with an
// alpha of its own, and the sum is clamped to the alpha.
fn gl_transition(uv: vec2f) -> vec4f {
    let source = gl_from_color(uv);
    let destination = gl_to_color(uv);
    let glow = builtin_premultiply(params.color).rgb;
    let lit_from = source.rgb + progress() * glow * source.a;
    let lit_to = destination.rgb + (1.0 - progress()) * glow * destination.a;
    return builtin_valid(vec4f(
        mix(lit_from, lit_to, progress()),
        mix(source.a, destination.a, progress()),
    ));
}
