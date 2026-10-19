fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    var position = uv;
    let keep_first = params.keep_second_half == 0u;
    if params.axis == 0u {
        position.x = select(max(position.x, 1.0 - position.x), min(position.x, 1.0 - position.x), keep_first);
    } else {
        position.y = select(max(position.y, 1.0 - position.y), min(position.y, 1.0 - position.y), keep_first);
    }
    return sample_src(position);
}
