// Port of gl-transitions/transitions/HSVfade.glsl
// Author: nwoeanhinnogaehr
// License: MIT
// alpha2: HSV of the straight color, alpha mixed instead of opaque. Hue and saturation mix as a
// chroma vector to stay continuous across the hue wrap and through grays. Saturation fades out
// below a tenth of white.
fn hsv_fade_hsv(color: vec4f) -> vec3f {
    let hsv = builtin_rgb_to_hsv(builtin_straight(color));
    return vec3f(hsv.x, hsv.y * builtin_smoothstep(0.0, 0.1, hsv.z), hsv.z);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let outgoing = gl_from_color(uv);
    let incoming = gl_to_color(uv);
    let a = hsv_fade_hsv(outgoing);
    let b = hsv_fade_hsv(incoming);
    let chroma_a = a.y * vec2f(cos(a.x * 6.28318531), sin(a.x * 6.28318531));
    let chroma_b = b.y * vec2f(cos(b.x * 6.28318531), sin(b.x * 6.28318531));
    let chroma = mix(chroma_a, chroma_b, progress());
    // No chroma, no hue: atan2(0, 0) is undefined.
    let hue = select(0.0, atan2(chroma.y, chroma.x) / 6.28318531, dot(chroma, chroma) > 0.0);
    let m = vec3f(hue, length(chroma), mix(a.z, b.z, progress()));
    let alpha = mix(outgoing.a, incoming.a, progress());
    return vec4f(builtin_hsv_to_rgb(m) * alpha, alpha);
}
