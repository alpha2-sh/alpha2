// Average-limit spill suppression: the screen's channel is held at most at the mean of the
// other two, by `amount`.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    var rgb = builtin_straight(src);
    if params.screen == 0u {
        rgb.g = mix(rgb.g, min(rgb.g, (rgb.r + rgb.b) * 0.5), params.amount);
    } else {
        rgb.b = mix(rgb.b, min(rgb.b, (rgb.r + rgb.g) * 0.5), params.amount);
    }
    return builtin_output(rgb, src.a);
}
