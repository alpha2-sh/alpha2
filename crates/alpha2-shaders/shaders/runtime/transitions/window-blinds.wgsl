// Port of gl-transitions/transitions/windowblinds.glsl
// Author: Fabien Benetou
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let stripe = uv.y * 50.0 * progress();
    let coverage = builtin_aa_pulse(stripe, 0.5, fwidthFine(stripe));
    let phase = progress() * (1.0 + 0.5 * coverage);
    let amount = mix(phase, progress(), smoothstep(0.8, 1.0, progress()));
    // `amount` exceeds 1 for p in (2/3, ~0.97), so clamp the extrapolated mix to premultiplied
    // range.
    let mixed = mix(gl_from_color(uv), gl_to_color(uv), amount);
    let alpha = clamp(mixed.a, 0.0, 1.0);
    return vec4f(clamp(mixed.rgb, vec3f(0.0), vec3f(alpha)), alpha);
}
