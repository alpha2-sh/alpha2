// Port of gl-transitions/transitions/Slides.glsl
// Author: Mark Craig
// License: MIT
// alpha2: `type` is the `origin` enum and `In` is `slide_in`; the slide's edges are
// antialiased.

// The top-left corner of the sliding picture, in y-down coordinates.
fn slides_corner(rad: f32) -> vec2f {
    let half = rad / 2.0;
    switch params.origin {
        case 0u: { return vec2f(0.5 - half, 0.0); }
        case 1u: { return vec2f(1.0 - rad, 0.5 - half); }
        case 2u: { return vec2f(0.5 - half, 1.0 - rad); }
        case 3u: { return vec2f(0.0, 0.5 - half); }
        case 4u: { return vec2f(1.0 - rad, 0.0); }
        case 5u: { return vec2f(1.0 - rad, 1.0 - rad); }
        case 6u: { return vec2f(0.0, 1.0 - rad); }
        case 7u: { return vec2f(0.0, 0.0); }
        default: { return vec2f(0.5 - half, 0.5 - half); }
    }
}

fn gl_transition(uv: vec2f) -> vec4f {
    let slide_in = params.slide_in != 0u;
    let rad = select(1.0 - progress(), progress(), slide_in);
    let corner = slides_corner(rad);
    let down = vec2f(uv.x, 1.0 - uv.y);
    let local = (down - corner) / rad;
    let width = 1.0 / (resolution() * rad);
    let inside = builtin_interval_coverage(local.x, width.x) * builtin_interval_coverage(local.y, width.y);
    let uv2 = vec2f(local.x, 1.0 - local.y);
    let moving = select(gl_from_color(uv2), gl_to_color(uv2), slide_in);
    let still = select(gl_to_color(uv), gl_from_color(uv), slide_in);
    return mix(still, moving, inside);
}
