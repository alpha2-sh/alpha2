// Drop shadow's first pass: the source's alpha moved by `offset` (uv), in the shadow color.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let coverage = sample_src(uv - params.offset).a * params.opacity * params.color.a;
    return builtin_premultiply(vec4f(params.color.rgb, 1.0)) * coverage;
}
