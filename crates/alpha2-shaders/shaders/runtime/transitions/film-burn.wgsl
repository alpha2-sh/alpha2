// Port of gl-transitions/transitions/FilmBurn.glsl
// Author: Anastasia Dunbar
// License: MIT
// alpha2: PCG hashes salted by the seed's bits replace upstream's `fract(sin(...))`; the
// thirteen flares are unrolled; the blur takes up to `max_filter_samples()` taps at evenly
// spaced angles (upstream's `degrees()` of an angle already in degrees scattered them); the
// light is added with an alpha of its own.
const FILM_BURN_PI: f32 = 3.14159265358979323;

// Upstream's sigmoid(x, 2) and sigmoid(x, 10), the powers multiplied out.
fn film_burn_sigmoid2(x: f32) -> f32 {
    let low = x * 2.0;
    let high = 2.0 - x * 2.0;
    return select(low * low / 2.0, 1.0 - high * high / 2.0, x > 0.5);
}

fn film_burn_sigmoid10(x: f32) -> f32 {
    let low = x * 2.0;
    let high = 2.0 - x * 2.0;
    let low4 = low * low * (low * low);
    let high4 = high * high * (high * high);
    return select(
        low4 * low4 * (low * low) / 2.0,
        1.0 - high4 * high4 * (high * high) / 2.0,
        x > 0.5,
    );
}

fn film_burn_seed() -> u32 {
    return bitcast<u32>(params.seed);
}

// Upstream's rand(i + offset): one value per flare and salt.
fn film_burn_rand(flare: u32, salt: u32) -> f32 {
    return builtin_unit(builtin_hash3(flare, salt, film_burn_seed()));
}

// Upstream's random(co, shift): a lattice value that blends into the next layer's as the
// shift grows.
fn film_burn_random(x: f32, y: f32, shift: f32) -> f32 {
    let layer = floor(shift);
    let a = builtin_unit(builtin_hash3(builtin_cell(x), builtin_cell(y), builtin_hash(builtin_cell(layer)) ^ film_burn_seed()));
    let b = builtin_unit(builtin_hash3(builtin_cell(x), builtin_cell(y), builtin_hash(builtin_cell(layer + 1.0)) ^ film_burn_seed()));
    return mix(a, b, film_burn_sigmoid2(shift - layer));
}

// Upstream's smooth_random(vec2(t), shift): value noise along the diagonal.
fn film_burn_noise(t: f32, shift: f32) -> f32 {
    let cell = floor(t);
    let w = film_burn_sigmoid2(t - cell);
    let bottom = mix(film_burn_random(cell, cell, shift), film_burn_random(cell + 1.0, cell, shift), w);
    let top = mix(film_burn_random(cell, cell + 1.0, shift), film_burn_random(cell + 1.0, cell + 1.0, shift), w);
    return mix(bottom, top, w);
}

// One flare: a moving interference pattern and a small bright spot.
fn film_burn_flare(p: vec2f, i: f32) -> f32 {
    let flare = u32(i);
    let r0 = film_burn_rand(flare, 0u);
    let r1 = film_burn_rand(flare, 1u);
    let r2 = film_burn_rand(flare, 2u);
    let r3 = film_burn_rand(flare, 3u);
    let wave = sin(((p.x * r0 * 6.0) + (progress() * 8.0)) + r1)
        * sin(((p.y * r2 * 6.0) + (progress() * 6.0)) + r3);
    let centre = vec2f(
        film_burn_noise(progress() * 1.3, i + 1.0),
        film_burn_noise(progress() * 0.5, i + 6.25),
    );
    let spot = 1.0 - clamp(length(p - centre) * mix(20.0, 70.0, r0), 0.0, 1.0);
    return wave + spot;
}

fn gl_transition(uv: vec2f) -> vec4f {
    var f = film_burn_flare(uv, 0.0);
    f += film_burn_flare(uv, 1.0);
    f += film_burn_flare(uv, 2.0);
    f += film_burn_flare(uv, 3.0);
    f += film_burn_flare(uv, 4.0);
    f += film_burn_flare(uv, 5.0);
    f += film_burn_flare(uv, 6.0);
    f += film_burn_flare(uv, 7.0);
    f += film_burn_flare(uv, 8.0);
    f += film_burn_flare(uv, 9.0);
    f += film_burn_flare(uv, 10.0);
    f += film_burn_flare(uv, 11.0);
    f += film_burn_flare(uv, 12.0);
    f = (f + 4.0) / 11.0;
    let s = sin(progress() * FILM_BURN_PI);
    // Upstream's pow3 with exponents (1, 2 - s, 1.3) on |f| (1, 0.7, 0.6).
    let light = vec3f(abs(f), pow(abs(f * 0.7), 2.0 - s), pow(abs(f * 0.6), 1.3)) * s;

    let zoom = 1.0 + film_burn_noise(progress() * 5.0, 6.3) * s * 0.05;
    let p = (uv - 0.5) * zoom + 0.5;
    let blur_amount = s * 0.03;
    let amount = film_burn_sigmoid10(progress());
    let pixel = vec2u(max(floor(uv * resolution()), vec2f(0.0)));
    let pixel_seed = builtin_hash3(pixel.x, pixel.y, film_burn_seed());
    let taps = u32(clamp(max_filter_samples(), 1.0, 50.0));
    var blurred = vec4f(0.0);
    for (var i = 0u; i < taps; i++) {
        // builtin_unit(builtin_hash(pixel_seed ^ i)) written out: no helper calls in a loop.
        let state = (pixel_seed ^ i) * 747796405u + 2891336453u;
        let word = ((state >> ((state >> 28u) + 4u)) ^ state) * 277803737u;
        let jitter = f32(((word >> 22u) ^ word) >> 8u) / 16777216.0;
        let angle = f32(i) / f32(taps) * 2.0 * FILM_BURN_PI;
        let tap = p + vec2f(cos(angle), sin(angle)) * (jitter + blur_amount) * blur_amount;
        let flipped = vec2f(tap.x, 1.0 - tap.y);
        blurred += mix(from_color(flipped), to_color(flipped), amount);
    }
    blurred /= f32(taps);
    let glow = max(light.r, max(light.g, light.b));
    return builtin_valid(vec4f(blurred.rgb + light, max(blurred.a, glow)));
}
