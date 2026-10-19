fn straight(c: vec4f) -> vec3f {
    if c.a <= 0.0001 { return vec3f(0.0); }
    return c.rgb / c.a;
}

// What of each pixel glows: its straight color above the threshold, opaque so
// the blur that follows spreads it evenly.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    return vec4f(max(straight(src) - vec3f(params.threshold), vec3f(0.0)), 1.0);
}
