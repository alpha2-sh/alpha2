// An outer stroke `width` wide: the most alpha within reach, on two rings of taps, drawn in
// `color` under the source.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 8.0, 64.0));
    let reach = params.width / builtin_aspect();
    var cover = src.a;
    for (var i = 0u; i < taps; i++) {
        let angle = f32(i) * (BUILTIN_TAU / f32(taps));
        let ring = select(1.0, 0.5, (i & 1u) == 1u);
        cover = max(cover, sample_src(uv + vec2f(cos(angle), sin(angle)) * reach * ring).a);
    }
    let stroke = builtin_premultiply(params.color) * cover;
    return src + stroke * (1.0 - src.a);
}
