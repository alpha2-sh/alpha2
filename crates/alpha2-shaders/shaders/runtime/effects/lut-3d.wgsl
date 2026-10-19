fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    if src.a <= 0.000001 {
        return vec4f(0.0);
    }
    let straight = src.rgb / src.a;
    let mapped = sample_lut(straight);
    return vec4f(mix(straight, mapped, params.strength) * src.a, src.a);
}
