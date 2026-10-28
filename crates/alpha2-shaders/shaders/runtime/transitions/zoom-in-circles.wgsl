// Port of gl-transitions/transitions/ZoomInCircles.glsl
// Author: dycm8009
// License: MIT
// alpha2: the rings' borders are antialiased: a pixel on one blends both rings.
fn zoom_ring(uv: vec2f, amount: f32, mixed: f32) -> vec4f {
    let p = vec2f(0.5) + (uv - vec2f(0.5)) * amount;
    return mix(gl_from_color(p), gl_to_color(p), mixed);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let ratio2 = vec2f(1.0, 1.0 / ratio());
    let r = 2.0 * ((uv - 0.5) * ratio2);
    let pro = progress() / 0.8;
    let late = pro > 1.0;
    let z = select(pro * 0.2, 0.2 + (pro - 1.0) * 5.0, late);
    let t = select(0.0, clamp((progress() - 0.8) / 0.07, 0.0, 1.0), late);
    let l = length(r);
    let beyond0 = builtin_aa_step(0.5 + z, l);
    let beyond1 = builtin_aa_step(0.8 + z * 1.5, l);
    let beyond2 = builtin_aa_step(1.2 + z * 2.5, l);
    let ring0 = zoom_ring(uv, 1.0, t);
    let ring1 = zoom_ring(uv, 1.0 - 0.15 * pro, t * 0.5);
    let ring2 = zoom_ring(uv, 1.0 - 0.2 * pro, t * 0.2);
    let ring3 = zoom_ring(uv, 1.0 - 0.25 * pro, t);
    return mix(ring0, mix(ring1, mix(ring2, ring3, beyond2), beyond1), beyond0);
}
