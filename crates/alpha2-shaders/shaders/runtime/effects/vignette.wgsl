fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let distance_from_center = distance(uv, vec2f(0.5)) * 1.41421356;
    let shade = 1.0 - params.amount * smoothstep(
        params.radius - params.softness,
        params.radius + params.softness,
        distance_from_center,
    );
    return vec4f(src.rgb * shade, src.a);
}
