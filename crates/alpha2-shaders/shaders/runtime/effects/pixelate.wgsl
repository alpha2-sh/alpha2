fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = resolution().x / resolution().y;
    if params.shape == 0u {
        let cells = vec2f(max(params.blocks, 2.0), max(params.blocks / aspect, 2.0));
        return sample_src((floor(uv * cells) + vec2f(0.5)) / cells);
    }
    // Pointy-top hexagons `1 / blocks` of the output wide, rounded to the nearest center in cube
    // coordinates; positions in output heights.
    let size = aspect / max(params.blocks, 2.0) / 1.7320508;
    let p = (uv - 0.5) * vec2f(aspect, 1.0) / size;
    let q = vec2f(p.x * 0.57735027 - p.y / 3.0, p.y * 0.66666667);
    let cube = vec3f(q.x, q.y, -q.x - q.y);
    var r = round(cube);
    let d = abs(r - cube);
    if d.x > d.y && d.x > d.z {
        r.x = -r.y - r.z;
    } else if d.y > d.z {
        r.y = -r.x - r.z;
    }
    let center = vec2f(1.7320508 * r.x + 0.8660254 * r.y, 1.5 * r.y) * size;
    return sample_src(center / vec2f(aspect, 1.0) + 0.5);
}
