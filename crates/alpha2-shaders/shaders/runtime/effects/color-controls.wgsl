// Grading in linear light: the straight color is decoded with the sRGB EOTF, graded, and
// encoded again. Exposure is in stops; temperature and tint are half a stop of gain per
// unit (red against blue, green); contrast scales the distance from middle grey (0.18) in
// stops; saturation is the distance from the BT.709 luminance.
fn builtin_straight(c: vec4f) -> vec3f {
    if c.a <= 0.0001 { return vec3f(0.0); }
    return c.rgb / c.a;
}

fn builtin_decode(encoded: f32) -> f32 {
    if encoded <= 0.04045 { return encoded / 12.92; }
    return pow((encoded + 0.055) / 1.055, 2.4);
}

fn builtin_encode(linear: f32) -> f32 {
    if linear <= 0.0031308 { return linear * 12.92; }
    return 1.055 * pow(linear, 1.0 / 2.4) - 0.055;
}

fn builtin_luminance(rgb: vec3f) -> f32 { return dot(rgb, vec3f(0.2126, 0.7152, 0.0722)); }

const BUILTIN_MIDDLE_GREY: f32 = 0.18;

fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    let encoded = builtin_straight(src);
    var rgb = vec3f(builtin_decode(encoded.r), builtin_decode(encoded.g), builtin_decode(encoded.b));
    rgb *= exp2(vec3f(params.exposure) + vec3f(params.temperature, params.tint, -params.temperature) * 0.5);
    rgb = BUILTIN_MIDDLE_GREY * pow(max(rgb / BUILTIN_MIDDLE_GREY, vec3f(1.0e-10)), vec3f(params.contrast));
    rgb = mix(vec3f(builtin_luminance(rgb)), rgb, params.saturation);
    rgb = clamp(rgb, vec3f(0.0), vec3f(1.0));
    let graded = vec3f(builtin_encode(rgb.r), builtin_encode(rgb.g), builtin_encode(rgb.b));
    return vec4f(graded * src.a, src.a);
}
