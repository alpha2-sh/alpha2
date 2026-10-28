// A sine wave travelling along `angle`, displacing across it. The cycles run so far are reduced
// to their fraction first: `sin` of a large argument has no portable precision, and the frame
// time grows without bound on a long timeline.
fn effect(uv: vec2f, _src: vec4f) -> vec4f {
    let aspect = builtin_aspect();
    let turn = radians(params.angle);
    let direction = vec2f(cos(turn), -sin(turn));
    let normal = vec2f(-direction.y, direction.x);
    let p = (uv - 0.5) * aspect;
    let cycles = fract(params.speed * time());
    let phase = BUILTIN_TAU * (dot(p, direction) / params.wavelength - cycles) + radians(params.phase);
    let q = p + normal * params.amplitude * sin(phase);
    let source = q / aspect + 0.5;
    return builtin_sample_edge(source, fwidthFine(source), params.edge);
}
