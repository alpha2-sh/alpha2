fn straight(c: vec4f) -> vec3f {
    if c.a <= 0.0001 { return vec3f(0.0); }
    return c.rgb / c.a;
}

// The source (`from`) with its blurred bloom (`to`) added, keeping the
// source's alpha. It ignores progress: the graph draws it at 0.5.
fn transition(uv: vec2f) -> vec4f {
    let src = from_color(uv);
    let rgb = clamp(straight(src) + to_color(uv).rgb * params.intensity, vec3f(0.0), vec3f(1.0));
    return vec4f(rgb * src.a, src.a);
}
