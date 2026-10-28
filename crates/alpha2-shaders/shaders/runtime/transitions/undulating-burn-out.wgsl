// Port of gl-transitions/transitions/undulatingBurnOut.glsl
// Author: pthrasher
// License: MIT
// Adapted by gre from https://gist.github.com/pthrasher/8e6226b215548ba12734

fn quadratic_in_out(t: f32) -> f32 {
    let p = 2.0 * t * t;
    return select(-p + 4.0 * t - 1.0, p, t < 0.5);
}

// The burning ring's radius at `p`: progress plus a wave around the centre.
fn wave(p: vec2f) -> f32 {
    let offset_from_center = p - params.center;
    // Degrees from 0 to 360, scaled by pi * 30 / 360.
    let degs = (atan2(offset_from_center.y, offset_from_center.x) * 57.29578 + 180.0) * 0.2617994;
    let x = progress();
    let eased = builtin_smoothstep(0.0, 1.0, x);
    let magnitude = mix(0.02, 0.09, eased);
    let offset = mix(40.0, 30.0, eased);
    return x + quadratic_in_out(sin(degs)) * magnitude * sin(x * offset);
}

// alpha2: upstream switches hard between three looks by `d`, the distance inside the ring:
// past the ring a smooth mix, a band that extrapolates the mix to -1 (clamped here to
// premultiplied range) and an inner band of `color` over from. Each switch is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    let r = wave(uv);
    let dist = distance(params.center, uv);
    let d = r - dist;
    let past_glow = builtin_aa_step(-0.005, d);
    let past_burn = builtin_aa_step(0.005, d);
    let past_band = builtin_aa_step(0.01, d);
    let source = gl_from_color(uv);
    let target_color = gl_to_color(uv);
    let smooth_mix = mix(source, target_color, builtin_smoothstep(-params.smoothness, 0.0, r - dist * (1.0 + params.smoothness)));
    let glow = builtin_valid(mix(source, target_color, -1.0));
    let burn = mix(source, builtin_premultiply(params.color), 0.75);
    return smooth_mix * (1.0 - past_glow + past_band) + glow * (past_glow - past_burn) + burn * (past_burn - past_band);
}
