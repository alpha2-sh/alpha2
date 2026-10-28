// Port of gl-transitions/transitions/Bounce.glsl
// Author: Adrian Purser
// License: MIT
// alpha2: the shadow is composited over the incoming picture, so it darkens it without making
// it translucent, and the drop edge is antialiased.
const BOUNCE_PI = 3.14159265358;

fn gl_transition(uv: vec2f) -> vec4f {
    let eased = sin(progress() * BOUNCE_PI / 2.0);
    let phase = progress() * BOUNCE_PI * params.bounces;
    let y = abs(cos(phase)) * (1.0 - eased);
    let d = uv.y - y;
    let dropped = builtin_aa_step(d, 0.0);
    let shadow_alpha = params.shadow_color.a;
    let fade = mix(
        (d / params.shadow_height) * shadow_alpha + (1.0 - shadow_alpha),
        1.0,
        builtin_smoothstep(0.95, 1.0, progress()),
    );
    let shadow = clamp(step(d, params.shadow_height) * (1.0 - fade), 0.0, 1.0);
    let shaded = builtin_over(vec4f(params.shadow_color.rgb, 1.0) * shadow, gl_to_color(uv));
    return mix(shaded, gl_from_color(vec2f(uv.x, uv.y + (1.0 - y))), dropped);
}
