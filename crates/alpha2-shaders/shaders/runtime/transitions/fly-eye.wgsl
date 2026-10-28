// Port of gl-transitions/transitions/flyeye.glsl
// Author: gre
// License: MIT
fn gl_transition(uv: vec2f) -> vec4f {
    let inv = 1.0 - progress();
    let displacement = params.size * vec2f(cos(params.zoom * uv.x), sin(params.zoom * uv.y));
    let to_tap = gl_to_color(uv + inv * displacement);
    let red = gl_from_color(uv + progress() * displacement * (1.0 - params.color_separation));
    let green = gl_from_color(uv + progress() * displacement);
    let blue = gl_from_color(uv + progress() * displacement * (1.0 + params.color_separation));
    // alpha2: the three taps' coverage, where upstream forced alpha 1.
    let from_tap = vec4f(red.r, green.g, blue.b, max(red.a, max(green.a, blue.a)));
    return to_tap * progress() + from_tap * inv;
}
