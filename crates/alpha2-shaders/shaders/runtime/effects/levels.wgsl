// Input range to 0..1, a gamma, then the output range, on the encoded color of all
// channels or one.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    let range = max(params.input_white - params.input_black, 0.0001);
    let x = clamp((rgb - vec3f(params.input_black)) / range, vec3f(0.0), vec3f(1.0));
    let y = pow(max(x, vec3f(1.0e-10)), vec3f(1.0 / params.gamma));
    let leveled = mix(vec3f(params.output_black), vec3f(params.output_white), y);
    var mask = vec3f(1.0);
    if params.channel == 1u { mask = vec3f(1.0, 0.0, 0.0); }
    if params.channel == 2u { mask = vec3f(0.0, 1.0, 0.0); }
    if params.channel == 3u { mask = vec3f(0.0, 0.0, 1.0); }
    return builtin_output(mix(rgb, leveled, mask), src.a);
}
