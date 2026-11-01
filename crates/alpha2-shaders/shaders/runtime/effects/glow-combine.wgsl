fn straight(c: vec4f) -> vec3f {
    if c.a <= 0.0001 { return vec3f(0.0); }
    return c.rgb / c.a;
}

// The source (`from`) plus its blurred bloom (`to`), keeping source alpha. Progress is ignored.
fn transition(uv: vec2f) -> vec4f {
    let src = from_color(uv);
    let rgb = clamp(straight(src) + to_color(uv).rgb * params.intensity, vec3f(0.0), vec3f(1.0));
    return vec4f(rgb * src.a, src.a);
}
