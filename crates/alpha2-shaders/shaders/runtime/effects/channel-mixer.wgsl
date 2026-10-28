// Each output channel from the input channels: a row is (red, green, blue, constant).
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let rgb = builtin_straight(src);
    var mixed = vec3f(
        dot(rgb, params.red.xyz) + params.red.w,
        dot(rgb, params.green.xyz) + params.green.w,
        dot(rgb, params.blue.xyz) + params.blue.w,
    );
    if params.monochrome != 0u { mixed = vec3f(mixed.r); }
    return builtin_output(mixed, src.a);
}
