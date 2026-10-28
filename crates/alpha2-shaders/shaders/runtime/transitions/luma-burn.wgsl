// Port of gl-transitions/transitions/dissolve.glsl
// Author: hjm1fb
// License: MIT
// alpha2: the burn works on straight color and premultiplies once (upstream premultiplied
// premultiplied color again), the burn color is clamped to 0..1, the colors' alphas are
// unused and upstream's unused noise is dropped.
fn gl_transition(uv: vec2f) -> vec4f {
    let source = gl_from_color(uv);
    let target_color = gl_to_color(uv);
    let straight = builtin_straight(source);
    let burn = 0.5 + 0.5 * (0.299 * straight.r + 0.587 * straight.g + 0.114 * straight.b);
    let show = burn - progress();
    let factor = 1.0 - builtin_smoothstep(0.0, params.line_width, show);
    let burn_color = clamp(
        pow(mix(params.spread_color.rgb, params.hot_color.rgb, factor), vec3f(params.power)) * params.intensity,
        vec3f(0.0),
        vec3f(1.0),
    );
    let burned = vec4f(mix(straight, burn_color, factor) * source.a, source.a);
    return select(burned, target_color, show < 0.001);
}
