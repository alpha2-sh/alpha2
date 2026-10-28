// The color mixed over the straight color, the color's alpha a strength; alpha is kept.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    return builtin_output(mix(rgb, params.color.rgb, params.amount * params.color.a), src.a);
}
