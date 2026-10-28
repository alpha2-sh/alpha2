// A disc of `radius` (output heights) sampled on Vogel's spiral: equal-area rings turned by
// the golden angle. `highlights` weights bright taps up so they bloom into bokeh discs.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 1.0, 64.0));
    let reach = params.radius / builtin_aspect();
    var sum = vec4f(0.0);
    var weight = 0.0;
    for (var i = 0u; i < taps; i++) {
        let r = sqrt((f32(i) + 0.5) / f32(taps));
        // The golden angle in turns, reduced before it reaches cos and sin.
        let a = fract(f32(i) * 0.381966011) * BUILTIN_TAU;
        let s = sample_src(uv + vec2f(cos(a), sin(a)) * r * reach);
        let l = dot(s.rgb, vec3f(0.2126, 0.7152, 0.0722));
        let w = 1.0 + params.highlights * l * l * l * l;
        sum += s * w;
        weight += w;
    }
    return sum / weight;
}
