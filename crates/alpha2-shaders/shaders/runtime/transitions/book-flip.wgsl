// Port of gl-transitions/transitions/BookFlip.glsl
// Author: hong
// License: MIT
// alpha2: only the half a pixel is on is sampled, the divisions are guarded where the page
// is edge-on, and the page edge is antialiased.

// `value`, kept away from zero on its own side.
fn book_flip_divisor(value: f32) -> f32 {
    return select(value, 0.0001, abs(value) < 0.0001);
}

fn book_flip_right(p: vec2f) -> vec2f {
    let skew_x = (p.x - progress()) / book_flip_divisor(0.5 - progress()) * 0.5;
    let skew_y = (p.y - 0.5) / (0.5 + progress() * (p.x - 0.5) / 0.5) * 0.5 + 0.5;
    return vec2f(skew_x, skew_y);
}

fn book_flip_left(p: vec2f) -> vec2f {
    let skew_x = (p.x - 0.5) / book_flip_divisor(progress() - 0.5) * 0.5 + 0.5;
    let skew_y = (p.y - 0.5) / (0.5 + (1.0 - progress()) * (0.5 - p.x) / 0.5) * 0.5 + 0.5;
    return vec2f(skew_x, skew_y);
}

fn gl_transition(p: vec2f) -> vec4f {
    let turned = builtin_aa_step(1.0 - progress(), p.x);
    let shade = vec4f(vec3f(max(0.7, abs(progress() - 0.5) * 2.0)), 1.0);
    let left = mix(gl_from_color(p), gl_to_color(book_flip_left(p)) * shade, turned);
    let right = mix(gl_from_color(book_flip_right(p)) * shade, gl_to_color(p), turned);
    return select(right, left, p.x < 0.5);
}
