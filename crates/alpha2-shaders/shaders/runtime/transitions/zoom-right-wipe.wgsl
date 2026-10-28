// Port of gl-transitions/transitions/ZoomRigthWipe.glsl
// Author: Handk
// License: MIT
// alpha2: upstream's global nQuick is computed here, its ramp is the helper's (safe at a
// quickness of 0), and the wipe's edge is antialiased.
fn gl_transition(uv: vec2f) -> vec4f {
    // The second half's wipe, first: derivatives need uniform control flow.
    let wiped = builtin_aa_step(uv.x, (progress() - 0.5) * 2.0);
    let quick = clamp(params.zoom_quickness, 0.0, 0.5);
    // The first half zooms the outgoing picture in to 2x and back out.
    let amount = builtin_smoothstep(0.0, quick, progress());
    let zoom = select(amount, 1.0 - amount, amount < 0.5);
    let zoomed = gl_from_color(0.5 + (uv - 0.5) * zoom);
    let wipe = mix(gl_from_color(uv), gl_to_color(uv), wiped);
    return select(wipe, zoomed, progress() < 0.5);
}
