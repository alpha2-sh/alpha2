// Port of gl-transitions/transitions/ButterflyWaveScrawler.glsl
// Author: mandubian
// License: MIT
// alpha2: `acos` is a polynomial over an argument clamped to its domain, and the from
// picture's alpha is the most of its three samples instead of 1.
const BUTTERFLY_PI: f32 = 3.14159265358979323846264;

// Abramowitz and Stegun 4.4.46, within 2e-8: `waves` scales the angle up to a hundredfold,
// more than the drivers' own `acos` approximations agree to.
fn butterfly_acos(value: f32) -> f32 {
    let x = abs(clamp(value, -1.0, 1.0));
    let polynomial = ((((((-0.0012624911 * x + 0.0066700901) * x - 0.0170881256) * x
        + 0.0308918810) * x - 0.0501743046) * x + 0.0889789874) * x - 0.2145988016) * x
        + 1.5707963050;
    let angle = sqrt(1.0 - x) * polynomial;
    return select(angle, BUTTERFLY_PI - angle, value < 0.0);
}

fn butterfly_compute(p: vec2f) -> f32 {
    let o = p * sin(progress() * params.amplitude) - vec2f(0.5);
    // The angle against the horizontal: dot(o, (1, 0)).
    let theta = butterfly_acos(o.x) * params.waves;
    let s = sin((2.0 * theta - BUTTERFLY_PI) / 24.0);
    let s2 = s * s;
    return (exp(cos(theta)) - 2.0 * cos(4.0 * theta) + s2 * s2 * s) / 10.0;
}

fn gl_transition(uv: vec2f) -> vec4f {
    let inv = 1.0 - progress();
    let disp = butterfly_compute(uv);
    let tex_to = gl_to_color(uv + inv * disp);
    let red = gl_from_color(uv + progress() * disp * (1.0 - params.color_separation));
    let green = gl_from_color(uv + progress() * disp);
    let blue = gl_from_color(uv + progress() * disp * (1.0 + params.color_separation));
    let tex_from = vec4f(red.r, green.g, blue.b, max(red.a, max(green.a, blue.a)));
    return tex_to * progress() + tex_from * inv;
}
