// Two soft blobs of light drifting slowly about `position`, screened over the picture. The
// source's alpha stays, so transparent pixels stay clear; at speed zero the blobs hold still.
fn effect(uv: vec2f, src: vec4f) -> vec4f {
    let seed = builtin_seed(params.seed);
    let t = time() * params.speed;
    let aspect = builtin_aspect();
    let drift_a = vec2f(builtin_value_noise(vec2f(t, 0.0), seed), builtin_value_noise(vec2f(t, 7.0), seed)) - 0.5;
    let drift_b = vec2f(builtin_value_noise(vec2f(t, 13.0), seed), builtin_value_noise(vec2f(t, 19.0), seed)) - 0.5;
    let a = (uv - params.position - drift_a * 0.3) * aspect;
    let b = (uv - params.position - vec2f(0.25, 0.1) - drift_b * 0.4) * aspect;
    let s2 = params.size * params.size;
    let glow = exp(-dot(a, a) / s2) + 0.6 * exp(-dot(b, b) / (s2 * 0.5));
    let leak = clamp(params.color.rgb * glow * params.intensity * params.color.a, vec3f(0.0), vec3f(1.0));
    let rgb = builtin_straight(src);
    return builtin_output(vec3f(1.0) - (vec3f(1.0) - rgb) * (vec3f(1.0) - leak), src.a);
}
