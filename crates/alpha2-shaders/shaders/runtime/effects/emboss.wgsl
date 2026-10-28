// Relief lit from `angle`: the luminance ahead minus the luminance behind, about mid gray.
// Premultiplied values, so the edges of the alpha stand out too.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let angle = radians(params.angle);
    let offset = vec2f(cos(angle), -sin(angle)) * params.distance / builtin_aspect();
    let ahead = sample_src(uv + offset);
    let behind = sample_src(uv - offset);
    let relief = params.strength * (builtin_luminance(ahead.rgb) - builtin_luminance(behind.rgb));
    let gray = vec3f(0.5 + relief);
    let colored = builtin_straight(src) + vec3f(relief);
    return builtin_output(select(gray, colored, params.keep_color != 0u), src.a);
}
