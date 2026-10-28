// White where the gray is above `level`, black below, ramped over `softness`.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let half_soft = params.softness * 0.5;
    let gray = builtin_gray(builtin_straight(src));
    let value = builtin_smoothstep(params.level - half_soft, params.level + half_soft, gray);
    return builtin_output(vec3f(value), src.a);
}
