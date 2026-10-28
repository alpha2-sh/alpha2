// Each pixel displaced by two channels of the `map` input, 0.5 being no displacement. A color
// channel of a transparent map is neutral; the alpha channel is taken as it is.
fn displacement_channel(c: vec4f, channel: u32) -> f32 {
    let rgb = builtin_straight(c);
    let color = select(select(select(builtin_luminance(rgb), rgb.b, channel == 2u), rgb.g, channel == 1u), rgb.r, channel == 0u);
    return select(mix(0.5, color, c.a), c.a, channel == 3u);
}

fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let m = sample_map(uv);
    let d = vec2f(displacement_channel(m, params.channel_x), displacement_channel(m, params.channel_y)) - 0.5;
    let source = uv + d * 2.0 * vec2f(params.amount_x, params.amount_y) / builtin_aspect();
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
