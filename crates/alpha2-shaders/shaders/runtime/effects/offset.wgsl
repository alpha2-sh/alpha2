// The picture shifted by `shift` of the output, what moves out coming back in per `edge`.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let source = uv - params.shift;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
