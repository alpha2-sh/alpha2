// Port of gl-transitions/transitions/WaterDrop.glsl
// Author: Paweł Płóciennik
// License: MIT
// alpha2: the drop's rim is antialiased; upstream switched hard between the two looks.
fn gl_transition(uv: vec2f) -> vec4f {
    let direction = uv - vec2f(0.5);
    let dist = length(direction);
    let outside = builtin_aa_step(progress(), dist);
    let offset = direction * sin(dist * params.amplitude - progress() * params.speed);
    let target_color = gl_to_color(uv);
    let still = mix(gl_from_color(uv), target_color, progress());
    let rippled = mix(gl_from_color(uv + offset), target_color, progress());
    return mix(rippled, still, outside);
}
