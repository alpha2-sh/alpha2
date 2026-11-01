// The picture repeated `tiles` times about the center. Repeated tiles can show seams where
// opposite edges differ; mirrored tiles cannot.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let p = (uv - 0.5 - params.offset) * params.tiles + 0.5;
    let mode = select(2u, 3u, params.mirror != 0u);
    return sample_src(builtin_wrap(p, mode));
}
