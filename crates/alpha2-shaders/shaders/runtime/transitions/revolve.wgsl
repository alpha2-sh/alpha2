// Port of gl-transitions/transitions/Revolve_Left.glsl
// Author: bread
// License: MIT
// alpha2: the angles are in degrees. The motion blur takes max_filter_samples() sub-steps of
// progress, each warped inline: a loop calls no helper of the body.
fn revolve_envelope(t: f32) -> f32 {
    let a = clamp((t - 0.10) / 0.33, 0.0, 1.0);
    let b = clamp((t - 0.43) / 0.29, 0.0, 1.0);
    return a * a * (3.0 - 2.0 * a) * (1.0 - b * b * (3.0 - 2.0 * b));
}

fn gl_transition(uv: vec2f) -> vec4f {
    let taps = u32(clamp(max_filter_samples(), 1.0, 64.0));
    let e0 = revolve_envelope(progress());
    let span = 0.060 * params.motion_blur * e0;
    let edge_turn = radians(params.max_rotation);
    let core_turn = radians(params.swirl);
    // The reveal is `builtin_smoothstep(switch_start, switch_end, t)`, written out for the loop.
    let switch_span = params.switch_end - params.switch_start;
    let hard = switch_span == 0.0;
    let inverse_span = select(1.0 / switch_span, 0.0, hard);
    var color = vec4f(0.0);
    var total = 0.0;
    for (var i = 0u; i < taps; i++) {
        let x = (f32(i) + 0.5) / f32(taps) * 2.0 - 1.0;
        let w0 = 1.0 - abs(x);
        let w = w0 * w0 + 0.01;
        let t = clamp(progress() + x * span, 0.0, 1.0);
        let rise = clamp((t - 0.10) / 0.33, 0.0, 1.0);
        let fall = clamp((t - 0.43) / 0.29, 0.0, 1.0);
        let e = rise * rise * (3.0 - 2.0 * rise) * (1.0 - fall * fall * (3.0 - 2.0 * fall));
        var p = (uv - params.center) * vec2f(ratio(), 1.0);
        let r = length(p);
        let core = pow(1.0 - clamp(r / 0.96, 0.0, 1.0), 1.55);
        let visible = params.direction * (edge_turn * e + core_turn * e * core);
        let c = cos(-visible);
        let s = sin(-visible);
        p = vec2f(c * p.x - s * p.y, s * p.x + c * p.y);
        p = p / (1.0 + (params.peak_zoom - 1.0) * pow(e, 0.85));
        let rr = length(p);
        p = p * (1.0 + params.barrel * e * rr * rr * 2.8);
        let q = clamp(vec2f(p.x / ratio(), p.y) + params.center, vec2f(0.001), vec2f(0.999));
        let k = clamp((t - params.switch_start) * inverse_span, 0.0, 1.0);
        let reveal = select(k * k * (3.0 - 2.0 * k), step(params.switch_start, t), hard);
        let flipped = vec2f(q.x, 1.0 - q.y);
        color += mix(from_color(flipped), to_color(flipped), reveal) * w;
        total += w;
    }
    color /= total;
    let centred = (uv - vec2f(0.5)) * vec2f(ratio(), 1.0);
    let vignette = 1.0 - params.shadow * e0 * builtin_smoothstep(0.35, 0.95, length(centred));
    return vec4f(color.rgb * vignette, color.a);
}
