// The red channel from one side, blue from the other: along `angle`, or away from the
// centre. Each channel comes with its own sample's alpha, so rgb stays within alpha.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let angle = radians(params.angle);
    let along = vec2f(cos(angle), -sin(angle)) * params.amount / builtin_aspect();
    let shift = select(along, (uv - vec2f(0.5)) * params.amount * 4.0, params.mode == 1u);
    let red = sample_src(uv + shift);
    let blue = sample_src(uv - shift);
    let alpha = max(src.a, max(red.a, blue.a));
    return vec4f(red.r, src.g, blue.b, alpha);
}
