// Port of gl-transitions/transitions/old_tv_lost_signal.glsl
// Author: mernking gitlab: Godswork
// License: MIT
// alpha2: a PCG hash of the line band and of the progress replaces upstream's
// `fract(sin(...))`, so the bands still flicker from frame to frame; the scanline
// darkening is scaled by the alpha, so the result stays premultiplied.

fn gl_transition(uv: vec2f) -> vec4f {
    let p = progress();
    let strength = sin(p * 3.14159265);
    var color = mix(gl_from_color(uv), gl_to_color(uv), p);
    // Horizontal tracking lines.
    let line_y = floor(uv.y * 120.0);
    let noise = builtin_unit(builtin_hash3(builtin_cell(line_y), bitcast<u32>(p), 0u));
    let line = step(0.92, noise);
    // The lines drift during the transition.
    let drift = sin(uv.y * 30.0 + p * 10.0) * 0.02 * strength;
    let shifted_from = gl_from_color(uv + vec2f(drift, 0.0));
    let shifted_to = gl_to_color(uv + vec2f(drift, 0.0));
    let line_color = mix(shifted_from, shifted_to, p);
    color = mix(color, line_color, line * strength);
    // Mild scanline darkening.
    let scan = sin(uv.y * 900.0) * 0.03;
    return builtin_valid(vec4f(color.rgb - vec3f(scan * strength * color.a), color.a));
}
