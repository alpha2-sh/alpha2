// The built-in chroma key. The key and each pixel are compared as straight,
// sRGB-encoded color. `distance_mode` 0 measures the RGB distance; mode 1 compares only
// the chroma plane, so a key survives luma differences such as shadows on a screen.
fn builtin_chroma_plane(rgb: vec3f) -> vec2f {
    let luma = dot(rgb, vec3f(0.2126, 0.7152, 0.0722));
    return vec2f(rgb.b - luma, rgb.r - luma);
}

fn effect(uv: vec2f, src: vec4f) -> vec4f {
    if src.a <= 0.000001 {
        return vec4f(0.0);
    }

    let straight_rgb = src.rgb / src.a;
    let key_color = params.key_color.rgb;
    var color_distance = distance(straight_rgb, key_color);
    if params.distance_mode == 1u {
        color_distance = distance(
            builtin_chroma_plane(straight_rgb),
            builtin_chroma_plane(key_color),
        );
    }

    let keep = smoothstep(params.threshold, params.threshold + params.softness, color_distance);
    let luminance = dot(straight_rgb, vec3f(0.2126, 0.7152, 0.0722));
    let corrected_rgb = mix(straight_rgb, vec3f(luminance), (1.0 - keep) * params.spill_suppression);
    let output_alpha = src.a * keep;
    return vec4f(corrected_rgb * output_alpha, output_alpha);
}
