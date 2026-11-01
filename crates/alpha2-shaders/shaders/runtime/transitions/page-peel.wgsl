// The outgoing picture curls onto a cylinder of `radius` (fraction of output height) along
// `angle`, seen from above: flat ahead of the fold, wrapping over the cylinder and lying face
// down behind it. The incoming picture shows where the sheet has gone.

fn transition(uv: vec2f) -> vec4f {
    let aspect = vec2f(resolution().x / resolution().y, 1.0);
    let angle = radians(params.angle);
    let d = vec2f(cos(angle), -sin(angle));
    let radius = max(params.radius, 0.0001);
    let half_turn = 3.14159265 * radius;
    // The fold starts on the frame's last corner along `d` and ends a radius past the first,
    // where the whole cylinder has left the frame.
    let c0 = dot(vec2f(0.0, 0.0), d);
    let c1 = dot(vec2f(aspect.x, 0.0), d);
    let c2 = dot(vec2f(0.0, 1.0), d);
    let c3 = dot(vec2f(aspect.x, 1.0), d);
    let start = min(min(c0, c1), min(c2, c3));
    let end = max(max(c0, c1), max(c2, c3)) + radius;
    let fold = mix(start, end, progress());
    let p = uv * aspect;
    // Signed distance ahead of the fold line.
    let x = dot(p, d) - fold;
    let turn = asin(clamp(-x / radius, 0.0, 1.0));
    // Arc length behind the fold of the face-up and face-down sheet points seen here.
    let front_arc = select(-x, radius * turn, x < 0.0);
    let back_arc = select(x + half_turn, radius * (3.14159265 - turn), x < 0.0);
    let front_uv = (p - d * (x + front_arc)) / aspect;
    let back_uv = (p - d * (x + back_arc)) / aspect;
    let front_w = fwidthFine(front_uv);
    let back_w = fwidthFine(back_uv);
    let front_in = builtin_interval_coverage(front_uv.x, front_w.x) * builtin_interval_coverage(front_uv.y, front_w.y);
    let back_in = builtin_interval_coverage(back_uv.x, back_w.x) * builtin_interval_coverage(back_uv.y, back_w.y);
    // Nothing of the sheet is more than a radius behind the fold.
    let sheet = builtin_aa_step(-radius, x);
    // Lit from the viewer: the cylinder darkens toward its silhouette.
    let light = select(1.0, 0.6 + 0.4 * abs(cos(turn)), x < 0.0);
    let front = from_color(front_uv);
    let back = from_color(back_uv);
    let whitened = mix(back, vec4f(back.a), params.back_shade);
    let incoming = to_color(uv);
    let face_up = mix(incoming, vec4f(front.rgb * light, front.a), front_in);
    let layered = mix(face_up, vec4f(whitened.rgb * light, whitened.a), back_in);
    return mix(incoming, layered, sheet);
}
