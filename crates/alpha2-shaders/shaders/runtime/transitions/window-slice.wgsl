// Port of gl-transitions/transitions/windowslice.glsl
// Author: gre
// License: MIT
fn window_slice_mask(x: f32) -> f32 {
    let phase = builtin_smoothstep(
        -params.smoothness,
        0.0,
        x - progress() * (1.0 + params.smoothness),
    );
    return step(phase, fract(params.count * x));
}

fn gl_transition(uv: vec2f) -> vec4f {
    // Integrate only the mask, keeping image details at pixel centers; this also handles
    // smoothness=0 and the seam at each slice.
    let count = clamp(max_filter_samples(), 1.0, 16.0);
    var mask = 0.0;
    for (var i = 0u; i < u32(count); i++) {
        let offset = ((f32(i) + 0.5) / count - 0.5) / resolution().x;
        mask += window_slice_mask(uv.x + offset);
    }
    mask /= count;
    return mix(gl_from_color(uv), gl_to_color(uv), mask);
}
