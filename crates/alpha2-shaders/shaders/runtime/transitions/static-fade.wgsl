// Port of gl-transitions/transitions/StaticFade.glsl
// Author: Ben Lucas
// License: MIT
// alpha2: PCG hashes replace upstream's `fract(sin(...))`: which cells show static is fixed,
// and their colors change with the progress. The static is opaque, as upstream.

fn static_fade_intensity(t: f32) -> f32 {
    let transition_progress = abs(2.0 * (t - 0.5));
    let threshold = 1.2 * (1.0 - transition_progress) - 0.1;
    return min(1.0, threshold);
}

fn gl_transition(uv: vec2f) -> vec4f {
    let base = select(gl_from_color(uv), gl_to_color(uv), progress() >= 0.5);
    let cell = floor(uv * params.n_noise_pixels);
    let x = builtin_cell(cell.x);
    let y = builtin_cell(cell.y);
    let frame = builtin_hash(bitcast<u32>(progress()));
    let noise = vec3f(
        builtin_unit(builtin_hash3(x, y, frame + 1u)),
        builtin_unit(builtin_hash3(x, y, frame + 2u)),
        builtin_unit(builtin_hash3(x, y, frame + 3u)),
    );
    let static_color = vec4f(params.static_luminosity * noise, 1.0);
    let chosen = builtin_unit(builtin_hash3(x, y, 0u)) <= static_fade_intensity(progress());
    return select(base, static_color, chosen);
}
