// Port of gl-transitions/transitions/EdgeTransition.glsl
// Author: Woohyun Kim
// License: MIT

// The 3 x 3 taps' edge strength times the centre tap, tap 3 * i + j at offset (i - 1, j - 1).
// alpha2: alpha is the centre tap's, where upstream forced 1. The taps are premultiplied, so
// alpha borders are edges too.
fn edge_color(c: array<vec3f, 9>, centre: vec4f) -> vec4f {
    let dx = 2.0 * abs(c[7] - c[1]) + abs(c[2] - c[6]) + abs(c[8] - c[0]);
    let dy = 2.0 * abs(c[3] - c[5]) + abs(c[6] - c[8]) + abs(c[0] - c[2]);
    let delta = length(0.25 * (dx + dy) * 0.5);
    return vec4f(clamp(params.edge_brightness * delta, 0.0, 1.0) * centre.rgb, centre.a);
}

fn gl_transition(uv: vec2f) -> vec4f {
    var from_taps: array<vec3f, 9>;
    var to_taps: array<vec3f, 9>;
    for (var i = 0; i < 3; i++) {
        for (var j = 0; j < 3; j++) {
            let p = uv + params.edge_thickness * vec2f(f32(i - 1), f32(j - 1));
            // The prelude samplers: a helper call in a loop costs the call budget.
            let flipped = vec2f(p.x, 1.0 - p.y);
            from_taps[3 * i + j] = from_color(flipped).rgb;
            to_taps[3 * i + j] = to_color(flipped).rgb;
        }
    }
    let source = gl_from_color(uv);
    let target_color = gl_to_color(uv);
    let start = mix(source, edge_color(from_taps, source), clamp(2.0 * progress(), 0.0, 1.0));
    let end = mix(edge_color(to_taps, target_color), target_color, clamp(2.0 * (progress() - 0.5), 0.0, 1.0));
    return mix(start, end, progress());
}
