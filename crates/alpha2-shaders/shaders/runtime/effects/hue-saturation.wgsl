// Hue turned and saturation scaled in HSV, then the color mixed toward white or black.
fn effect(_uv: vec2f, src: vec4f) -> vec4f {
    var hsv = builtin_rgb_to_hsv(builtin_straight(src));
    hsv.x += params.hue / 360.0;
    hsv.y = clamp(hsv.y * (1.0 + params.saturation), 0.0, 1.0);
    var rgb = builtin_hsv_to_rgb(hsv);
    let toward = select(vec3f(0.0), vec3f(1.0), params.lightness > 0.0);
    rgb = mix(rgb, toward, abs(params.lightness));
    return builtin_output(rgb, src.a);
}
