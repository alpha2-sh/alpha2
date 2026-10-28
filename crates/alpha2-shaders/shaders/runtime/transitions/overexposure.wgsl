// Port of gl-transitions/transitions/Overexposure.glsl
// Author: Ben Zhang
// License: MIT
// alpha2: the pictures are already premultiplied, so upstream's `* a` is dropped, and the
// overexposed sum is clamped to the alpha.
fn gl_transition(uv: vec2f) -> vec4f {
    let source = gl_from_color(uv);
    let destination = gl_to_color(uv);
    let flash = sin(3.141592653589793 * progress()) * params.strength;
    let from_m = 1.0 - progress() + flash;
    let to_m = progress() + flash;
    return builtin_valid(vec4f(
        source.rgb * from_m + destination.rgb * to_m,
        mix(source.a, destination.a, progress()),
    ));
}
