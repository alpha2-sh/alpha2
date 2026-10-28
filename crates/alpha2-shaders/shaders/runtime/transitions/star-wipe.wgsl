// Port of gl-transitions/transitions/StarWipe.glsl
// Author: Ben Lucas
// License: MIT
// alpha2: the inside test is a signed value, antialiased; border_color is premultiplied.
fn star_rotate(v: vec2f, theta: f32) -> vec2f {
    let cos_theta = cos(theta);
    let sin_theta = sin(theta);
    return vec2f(cos_theta * v.x - sin_theta * v.y, sin_theta * v.x + cos_theta * v.y);
}

// Upstream's inStar(uv, center, radius) is radius + star_value(uv, center) > 0. Neighbouring
// sectors mirror each other, so the value is continuous across their seams.
fn star_value(uv: vec2f, center: vec2f) -> f32 {
    let star_angle = 1.2566370614359172;
    let centered = star_rotate(uv - center, params.star_rotation * star_angle);
    let theta = atan2(centered.y, centered.x) + 3.141592653589793;
    let rotated = star_rotate(centered, -star_angle * (floor(theta / star_angle) + 0.5));
    return rotated.x * 0.3 - abs(rotated.y);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let thickness = params.border_thickness;
    let scaled = (2.0 * thickness + 1.0) * progress() - thickness;
    let value = star_value(uv, params.star_center);
    let inner = builtin_aa_step(-scaled, value);
    let outer = builtin_aa_step(-(scaled + thickness), value);
    let bordered = mix(gl_from_color(uv), builtin_premultiply(params.border_color), outer);
    return mix(bordered, gl_to_color(uv), inner);
}
