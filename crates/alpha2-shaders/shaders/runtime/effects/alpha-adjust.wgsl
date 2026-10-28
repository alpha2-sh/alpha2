// Levels on the alpha alone: `black` and `white` stretched to 0 and 1, then a gamma, then
// optionally inverted (transparent pixels become opaque straight black).
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let x = clamp((src.a - params.black) / max(params.white - params.black, 0.0001), 0.0, 1.0);
    // pow of zero is undefined, and the floor raised to a small exponent is not small: zero
    // stays zero.
    let curved = select(0.0, pow(max(x, 1.0e-10), 1.0 / params.gamma), x > 0.0);
    let alpha = select(curved, 1.0 - curved, params.invert != 0u);
    return builtin_output(rgb, alpha);
}
