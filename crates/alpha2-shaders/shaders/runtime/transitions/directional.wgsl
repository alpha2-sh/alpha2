// Port of gl-transitions/transitions/Directional.glsl
// Author: Gaëtan Renaudeau
// License: MIT
// The schema admits (0, 0), which moves neither clip: take the default direction instead.
fn directional_step() -> vec2f {
    let step = sign(params.direction);
    if all(step == vec2f(0.0)) { return vec2f(0.0, 1.0); }
    return step;
}

fn directional_sample(uv: vec2f) -> vec4f {
    let p = uv + progress() * directional_step();
    let wrapped = fract(p);
    let inside = step(0.0, p.y) * step(p.y, 1.0) * step(0.0, p.x) * step(p.x, 1.0);
    return mix(gl_to_color(wrapped), gl_from_color(wrapped), inside);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let p = uv + progress() * directional_step();
    let width = 1.0 / resolution();
    let coverage = builtin_interval_coverage(p.x, width.x) * builtin_interval_coverage(p.y, width.y);
    if coverage <= 0.0 || coverage >= 1.0 { return directional_sample(uv); }
    // The two images wrap at the seam: integrate subpixel samples there instead of
    // blending samples from the wrong side of the wrapped texture.
    let side = floor(sqrt(clamp(max_filter_samples(), 1.0, 16.0)));
    var color = vec4f(0.0);
    for (var y = 0u; y < u32(side); y++) {
        for (var x = 0u; x < u32(side); x++) {
            let offset = ((vec2f(f32(x), f32(y)) + 0.5) / side - 0.5) * width;
            color += directional_sample(uv + offset);
        }
    }
    return color / (side * side);
}
