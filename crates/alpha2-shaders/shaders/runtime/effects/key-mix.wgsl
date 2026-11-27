fn builtin_key_mix(backdrop: vec4f, picture: vec4f, coverage: f32) -> vec4f {
    let k = clamp(coverage, 0.0, 1.0);
    if (k <= 0.0) { return backdrop; }
    if (k >= 1.0) { return picture; }
    return backdrop * (1.0 - k) + picture * k;
}
