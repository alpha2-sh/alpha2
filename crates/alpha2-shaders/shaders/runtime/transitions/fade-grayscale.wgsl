// Port of gl-transitions/transitions/fadegrayscale.glsl
// Author: gre
// License: MIT
fn grayscale(color: vec3f) -> vec3f {
    return vec3f(dot(color, vec3f(0.2126, 0.7152, 0.0722)));
}

fn gl_transition(uv: vec2f) -> vec4f {
    let source_color = gl_from_color(uv);
    let target_color = gl_to_color(uv);
    let gray_from = vec4f(grayscale(source_color.rgb), source_color.a);
    let gray_to = vec4f(grayscale(target_color.rgb), target_color.a);
    let from_mix = mix(gray_from, source_color, 1.0 - builtin_smoothstep(0.0, 1.0 - params.intensity, progress()));
    let to_mix = mix(gray_to, target_color, builtin_smoothstep(params.intensity, 1.0, progress()));
    return mix(from_mix, to_mix, progress());
}
