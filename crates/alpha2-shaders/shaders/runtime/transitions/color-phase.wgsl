// Port of gl-transitions/transitions/colorphase.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let amount = builtin_smoothstep4(params.from_step, params.to_step, progress());
    let source = gl_from_color(uv);
    let destination = gl_to_color(uv);
    // Each channel has its own coverage during the color phase. Recover its color
    // before associating it with the common output alpha.
    let coverage = mix(vec3f(source.a), vec3f(destination.a), amount.rgb);
    let associated = mix(source.rgb, destination.rgb, amount.rgb);
    let rgb = select(vec3f(0.0), associated / max(coverage, vec3f(0.000001)), coverage > vec3f(0.000001));
    let alpha = mix(source.a, destination.a, amount.a);
    return vec4f(rgb * alpha, alpha);
}
