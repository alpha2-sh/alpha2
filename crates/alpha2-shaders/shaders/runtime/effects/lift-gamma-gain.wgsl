// On the encoded color: gain multiplies, lift raises black with white held, gamma is
// the exponent's inverse. Each wheel is an rgb offset plus a master in w.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let lift = params.lift.rgb + vec3f(params.lift.w);
    let gain = vec3f(1.0) + params.gain.rgb + vec3f(params.gain.w);
    let gamma = max(vec3f(1.0) + params.gamma.rgb + vec3f(params.gamma.w), vec3f(0.01));
    let graded = rgb * gain + lift * (vec3f(1.0) - rgb);
    let curved = pow(max(graded, vec3f(1.0e-10)), vec3f(1.0) / gamma);
    return builtin_output(curved, src.a);
}
