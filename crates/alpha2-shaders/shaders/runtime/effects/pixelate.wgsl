fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = resolution().x / resolution().y;
    let cells = vec2f(max(params.blocks, 2.0), max(params.blocks / aspect, 2.0));
    return sample_src((floor(uv * cells) + vec2f(0.5)) / cells);
}
