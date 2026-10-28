//! The "Stylized" group's kernels.

use core::f32::consts::{PI, TAU};

use libm::{atan2f, cosf, expf};

use super::{
    FRAC_PI_2, KernelContext, Rgba, Transition, Vec2, add, add4, builtin_aa_step,
    builtin_interval_coverage, builtin_premultiply, builtin_smoothstep, builtin_smoothstep4,
    builtin_straight, distance, dot, fract, fwidth_fine, gl_from_color, gl_to_color, gl_uv,
    l1_direction, length, mix, mix4, polynomial_sin, powf, scale, select, sinf, smoothstep, sub,
    transition_row,
};
use crate::cpu::kernels::{
    builtin_aa_smoothstep, builtin_cell, builtin_floor_mod, builtin_hash, builtin_hash3,
    builtin_unit, builtin_valid, builtin_value_noise, step,
};

pub(super) static KERNELS: &[(&str, Transition)] = &[
    ("color-phase", Transition(color_phase)),
    ("fade-color", Transition(fade_color)),
    ("fade-grayscale", Transition(fade_grayscale)),
    ("morph", Transition(morph)),
    ("ripple", Transition(ripple)),
    ("squeeze", Transition(squeeze)),
    ("simple-zoom", Transition(simple_zoom)),
    ("circle-crop", Transition(circle_crop)),
    ("colour-distance", Transition(colour_distance)),
    ("polka-dots-curtain", Transition(polka_dots_curtain)),
    ("directional-warp", Transition(directional_warp)),
    ("block-dissolve", Transition(block_dissolve)),
    ("random-squares", Transition(random_squares)),
    ("burn-noise", Transition(burn_noise)),
    ("perlin", Transition(perlin)),
    ("film-burn", Transition(film_burn)),
    ("overexposure", Transition(overexposure)),
    ("advanced-mosaic", Transition(advanced_mosaic)),
    ("burn", Transition(burn)),
    ("crosshatch", Transition(crosshatch)),
    (
        "butterfly-wave-scrawler",
        Transition(butterfly_wave_scrawler),
    ),
    ("crazy-parametric-fun", Transition(crazy_parametric_fun)),
    ("edge", Transition(edge)),
    ("fly-eye", Transition(fly_eye)),
    ("hexagonalize", Transition(hexagonalize)),
    ("kaleidoscope", Transition(kaleidoscope)),
    ("luma-burn", Transition(luma_burn)),
    ("luminance-melt", Transition(luminance_melt)),
    ("power-kaleido", Transition(power_kaleido)),
    ("random-noise", Transition(random_noise)),
    ("swirl", Transition(swirl)),
    ("undulating-burn-out", Transition(undulating_burn_out)),
    ("water-drop", Transition(water_drop)),
];

fn color_phase(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let amount = builtin_smoothstep4(
        context.vec4("from_step"),
        context.vec4("to_step"),
        context.progress(),
    );
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source = gl_from_color(context, uv);
        let destination = gl_to_color(context, uv);
        // Each channel has its own coverage during the colour phase: recover
        // its colour before associating it with the common alpha.
        let mut rgb = [0.0; 3];
        for (channel, value) in rgb.iter_mut().enumerate() {
            let coverage = mix(source[3], destination[3], amount[channel]);
            let associated = mix(source[channel], destination[channel], amount[channel]);
            *value = select(
                0.0,
                associated / coverage.max(0.000_001),
                coverage > 0.000_001,
            );
        }
        let alpha = mix(source[3], destination[3], amount[3]);
        [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
    });
}

fn fade_color(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let color = builtin_premultiply(context.vec4("color"));
    let color_phase = context.float("color_phase");
    let from_amount = 1.0 - builtin_smoothstep(0.0, 1.0 - color_phase, progress);
    let to_amount = builtin_smoothstep(color_phase, 1.0, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let from_mix = mix4(color, gl_from_color(context, uv), from_amount);
        let to_mix = mix4(color, gl_to_color(context, uv), to_amount);
        mix4(from_mix, to_mix, progress)
    });
}

fn grayscale(color: Rgba) -> f32 {
    color[0] * 0.2126 + color[1] * 0.7152 + color[2] * 0.0722
}

fn fade_grayscale(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let intensity = context.float("intensity");
    let from_amount = 1.0 - builtin_smoothstep(0.0, 1.0 - intensity, progress);
    let to_amount = builtin_smoothstep(intensity, 1.0, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source_color = gl_from_color(context, uv);
        let target_color = gl_to_color(context, uv);
        let gray = grayscale(source_color);
        let gray_from = [gray, gray, gray, source_color[3]];
        let gray = grayscale(target_color);
        let gray_to = [gray, gray, gray, target_color[3]];
        let from_mix = mix4(gray_from, source_color, from_amount);
        let to_mix = mix4(gray_to, target_color, to_amount);
        mix4(from_mix, to_mix, progress)
    });
}

/// `morph`'s offset of one picture's colour: zero where it is transparent.
fn morph_offset(color: Rgba) -> Vec2 {
    if color[3] > 0.000_001 {
        let straight = builtin_straight(color);
        [
            straight[0] + straight[2] - 1.0,
            straight[1] + straight[2] - 1.0,
        ]
    } else {
        [0.0, 0.0]
    }
}

fn morph(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let strength = context.float("strength");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let from_offset = morph_offset(gl_from_color(context, uv));
        let to_offset = morph_offset(gl_to_color(context, uv));
        let offset = scale(
            [
                mix(from_offset[0], to_offset[0], 0.5),
                mix(from_offset[1], to_offset[1], 0.5),
            ],
            strength,
        );
        mix4(
            gl_from_color(context, add(uv, scale(offset, progress))),
            gl_to_color(context, sub(uv, scale(offset, 1.0 - progress))),
            progress,
        )
    });
}

fn ripple(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let amplitude = context.float("amplitude");
    let speed = context.float("speed");
    let amount = smoothstep(0.2, 1.0, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let direction = sub(uv, [0.5, 0.5]);
        let distance_from_center = length(direction);
        let wave = sinf(progress * distance_from_center * amplitude - progress * speed) + 0.5;
        let offset = [
            direction[0] * wave / 30.0 * progress,
            direction[1] * wave / 30.0 * progress,
        ];
        mix4(
            gl_from_color(context, add(uv, offset)),
            gl_to_color(context, uv),
            amount,
        )
    });
}

fn squeeze(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let color_separation = context.float("color_separation");
    let squeezed = |uv: Vec2| 0.5 + (uv[1] - 0.5) / (1.0 - progress);
    let offset = [0.0, progress * color_separation];
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| squeezed(gl_uv(uv)));
        let uv = gl_uv(uv);
        let squeezed_y = squeezed(uv);
        let coverage = builtin_interval_coverage(squeezed_y, width);
        if coverage <= 0.0 {
            return gl_to_color(context, uv);
        }
        let sample_uv = [uv[0], squeezed_y];
        let center = gl_from_color(context, sample_uv);
        let negative = gl_from_color(context, sub(sample_uv, offset));
        let positive = gl_from_color(context, add(sample_uv, offset));
        // Cover all three displaced channels, fringes outside the centre
        // sample included.
        let alpha = center[3].max(negative[3].max(positive[3]));
        mix4(
            gl_to_color(context, uv),
            [negative[0], center[1], positive[2], alpha],
            coverage,
        )
    });
}

fn simple_zoom(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let quickness = context.float("zoom_quickness").clamp(0.2, 1.0);
    let zoom = 1.0 - smoothstep(0.0, quickness, progress);
    let amount = smoothstep(quickness - 0.2, 1.0, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let zoomed = add([0.5, 0.5], scale(sub(uv, [0.5, 0.5]), zoom));
        mix4(
            gl_from_color(context, zoomed),
            gl_to_color(context, uv),
            amount,
        )
    });
}

fn circle_crop(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let back_color = builtin_premultiply(context.vec4("back_color"));
    let ratio_scale = [1.0, 1.0 / context.ratio()];
    let radius = powf(2.0 * (progress - 0.5).abs(), 3.0);
    let distance_from_center = |uv: Vec2| {
        let centered = sub(uv, [0.5, 0.5]);
        length([centered[0] * ratio_scale[0], centered[1] * ratio_scale[1]])
    };
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| {
            distance_from_center(gl_uv(uv)) - radius
        });
        let uv = gl_uv(uv);
        let image = if progress >= 0.5 {
            gl_to_color(context, uv)
        } else {
            gl_from_color(context, uv)
        };
        let coverage = (1.0 - builtin_aa_step(radius, distance_from_center(uv), width))
            * select(0.0, 1.0, radius > 0.0);
        mix4(back_color, image, coverage)
    });
}

/// Softened over a fixed width of colour distance, not a pixel footprint.
const COLOUR_DISTANCE_SOFTNESS: f32 = 0.02;

fn colour_distance(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let to_amount = powf(progress, context.float("power"));
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source_color = gl_from_color(context, uv);
        let target_color = gl_to_color(context, uv);
        let a = builtin_straight(source_color);
        let b = builtin_straight(target_color);
        let difference = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let colour_distance = (difference[0] * difference[0]
            + difference[1] * difference[1]
            + difference[2] * difference[2])
            .sqrt();
        let mask = builtin_smoothstep(
            colour_distance - COLOUR_DISTANCE_SOFTNESS,
            colour_distance + COLOUR_DISTANCE_SOFTNESS,
            progress,
        );
        mix4(
            mix4(source_color, target_color, mask),
            target_color,
            to_amount,
        )
    });
}

fn polka_dots_curtain(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let dots = context.float("dots");
    let center = context.vec2("center");
    // The step's edge and the value it compares.
    let terms = |uv: Vec2| {
        let cells = scale(uv, dots);
        let cell_distance = distance([fract(cells[0]), fract(cells[1])], [0.5, 0.5]);
        let center_distance = distance(uv, center).max(0.000_001);
        (progress / center_distance, cell_distance)
    };
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| {
            let (edge, value) = terms(gl_uv(uv));
            value - edge
        });
        let uv = gl_uv(uv);
        let (edge, value) = terms(uv);
        let mask = 1.0 - builtin_aa_step(edge, value, width);
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

fn directional_warp(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let smoothness = context.float("smoothness");
    let vector = l1_direction(context.vec2("direction"), [-1.0, 1.0]);
    let edge = dot(vector, [0.5, 0.5]);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mask = 1.0
            - smoothstep(
                -smoothness,
                0.0,
                dot(vector, uv) - (edge - 0.5 + progress * (1.0 + smoothness)),
            );
        let from_uv = add(scale(sub(uv, [0.5, 0.5]), 1.0 - mask), [0.5, 0.5]);
        let to_uv = add(scale(sub(uv, [0.5, 0.5]), mask), [0.5, 0.5]);
        mix4(
            gl_from_color(context, from_uv),
            gl_to_color(context, to_uv),
            mask,
        )
    });
}

/// `block-dissolve.wgsl`.
fn block_dissolve(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let block_size = context.float("block_size");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let block = [uv[0] / block_size, uv[1] / block_size];
        let threshold = builtin_unit(builtin_hash3(
            builtin_cell(block[0]),
            builtin_cell(block[1]),
            0,
        ));
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            step(threshold, progress),
        )
    });
}

/// `random-squares.wgsl`.
fn random_squares(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let size = context.vec2("size").map(|size| size.floor().max(1.0));
    let smoothness = context.float("smoothness");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let cells = [size[0] * uv[0], size[1] * uv[1]];
        let threshold = builtin_unit(builtin_hash3(
            builtin_cell(cells[0]),
            builtin_cell(cells[1]),
            0,
        ));
        let mask = builtin_smoothstep(0.0, -smoothness, threshold - progress * (1.0 + smoothness));
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

/// `burn-noise.wgsl`.
fn burn_noise(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let burn_color = builtin_premultiply(context.vec4("burn_color"));
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source = gl_from_color(context, uv);
        let destination = gl_to_color(context, uv);
        let p = scale(uv, 4.0);
        let n = 0.5 * builtin_value_noise(p, 0)
            + 0.25 * builtin_value_noise(scale(p, 2.0), 1)
            + 0.125 * builtin_value_noise(scale(p, 4.0), 2)
            + 0.0625 * builtin_value_noise(scale(p, 8.0), 3);
        let l = builtin_smoothstep(progress, progress + 0.05, n);
        let edge = (1.0 - l) * l * 5.0;
        let mixed = mix4(destination, source, l);
        let glow = burn_color.map(|channel| channel * edge);
        builtin_valid([
            mixed[0] + glow[0],
            mixed[1] + glow[1],
            mixed[2] + glow[2],
            mixed[3].max(glow[3]),
        ])
    });
}

/// `perlin.wgsl`.
fn perlin(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let noise_scale = context.float("scale");
    let smoothness = context.float("smoothness");
    let seed = context.float("seed").to_bits();
    let noise = |uv: Vec2| builtin_value_noise(scale(uv, noise_scale), seed);
    let p = mix(-smoothness, 1.0 + smoothness, progress);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| noise(gl_uv(uv)));
        let uv = gl_uv(uv);
        let q = builtin_aa_smoothstep(p - smoothness, p + smoothness, noise(uv), width);
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            1.0 - q,
        )
    });
}

/// Upstream's `sigmoid(x, 2)`.
fn film_burn_sigmoid2(x: f32) -> f32 {
    let low = x * 2.0;
    let high = 2.0 - x * 2.0;
    select(low * low / 2.0, 1.0 - high * high / 2.0, x > 0.5)
}

/// Upstream's `sigmoid(x, 10)`.
fn film_burn_sigmoid10(x: f32) -> f32 {
    let low = x * 2.0;
    let high = 2.0 - x * 2.0;
    let low4 = low * low * (low * low);
    let high4 = high * high * (high * high);
    select(
        low4 * low4 * (low * low) / 2.0,
        1.0 - high4 * high4 * (high * high) / 2.0,
        x > 0.5,
    )
}

/// `film_burn_random`: a lattice value blending into the next layer's.
fn film_burn_random(seed: u32, x: f32, y: f32, shift: f32) -> f32 {
    let layer = shift.floor();
    let a = builtin_unit(builtin_hash3(
        builtin_cell(x),
        builtin_cell(y),
        builtin_hash(builtin_cell(layer)) ^ seed,
    ));
    let b = builtin_unit(builtin_hash3(
        builtin_cell(x),
        builtin_cell(y),
        builtin_hash(builtin_cell(layer + 1.0)) ^ seed,
    ));
    mix(a, b, film_burn_sigmoid2(shift - layer))
}

/// `film_burn_noise`: value noise along the diagonal.
fn film_burn_noise(seed: u32, t: f32, shift: f32) -> f32 {
    let cell = t.floor();
    let w = film_burn_sigmoid2(t - cell);
    let bottom = mix(
        film_burn_random(seed, cell, cell, shift),
        film_burn_random(seed, cell + 1.0, cell, shift),
        w,
    );
    let top = mix(
        film_burn_random(seed, cell, cell + 1.0, shift),
        film_burn_random(seed, cell + 1.0, cell + 1.0, shift),
        w,
    );
    mix(bottom, top, w)
}

/// One flare's values that do not depend on the pixel.
struct Flare {
    r0: f32,
    r1: f32,
    r2: f32,
    r3: f32,
    centre: Vec2,
}

fn film_burn(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let seed = context.float("seed").to_bits();
    let rand = |flare: u32, salt: u32| builtin_unit(builtin_hash3(flare, salt, seed));
    let flares: [Flare; 13] = core::array::from_fn(|index| {
        let flare = index as u32;
        let i = index as f32;
        Flare {
            r0: rand(flare, 0),
            r1: rand(flare, 1),
            r2: rand(flare, 2),
            r3: rand(flare, 3),
            centre: [
                film_burn_noise(seed, progress * 1.3, i + 1.0),
                film_burn_noise(seed, progress * 0.5, i + 6.25),
            ],
        }
    });
    let s = sinf(progress * PI);
    let zoom = 1.0 + film_burn_noise(seed, progress * 5.0, 6.3) * s * 0.05;
    let blur_amount = s * 0.03;
    let amount = film_burn_sigmoid10(progress);
    let taps = context.max_filter_samples().clamp(1.0, 50.0) as u32;
    let resolution = context.resolution();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut f = 0.0;
        for (index, flare) in flares.iter().enumerate() {
            let wave = sinf(((uv[0] * flare.r0 * 6.0) + (progress * 8.0)) + flare.r1)
                * sinf(((uv[1] * flare.r2 * 6.0) + (progress * 6.0)) + flare.r3);
            let spot =
                1.0 - (length(sub(uv, flare.centre)) * mix(20.0, 70.0, flare.r0)).clamp(0.0, 1.0);
            // The shader's first flare starts the sum.
            f = if index == 0 {
                wave + spot
            } else {
                f + (wave + spot)
            };
        }
        f = (f + 4.0) / 11.0;
        let light = [
            f.abs() * s,
            powf((f * 0.7).abs(), 2.0 - s) * s,
            powf((f * 0.6).abs(), 1.3) * s,
        ];
        let p = add(scale(sub(uv, [0.5, 0.5]), zoom), [0.5, 0.5]);
        let pixel = [
            (uv[0] * resolution[0]).floor().max(0.0) as u32,
            (uv[1] * resolution[1]).floor().max(0.0) as u32,
        ];
        let pixel_seed = builtin_hash3(pixel[0], pixel[1], seed);
        let mut blurred = [0.0; 4];
        for i in 0..taps {
            let jitter = builtin_unit(builtin_hash(pixel_seed ^ i));
            let angle = i as f32 / taps as f32 * 2.0 * PI;
            let tap = add(
                p,
                [
                    cosf(angle) * (jitter + blur_amount) * blur_amount,
                    sinf(angle) * (jitter + blur_amount) * blur_amount,
                ],
            );
            blurred = add4(
                blurred,
                mix4(
                    gl_from_color(context, tap),
                    gl_to_color(context, tap),
                    amount,
                ),
            );
        }
        let blurred = blurred.map(|channel| channel / taps as f32);
        let glow = light[0].max(light[1].max(light[2]));
        builtin_valid([
            blurred[0] + light[0],
            blurred[1] + light[1],
            blurred[2] + light[2],
            blurred[3].max(glow),
        ])
    });
}

/// `overexposure.wgsl`.
fn overexposure(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let flash = sinf(PI * progress) * context.float("strength");
    let from_m = 1.0 - progress + flash;
    let to_m = progress + flash;
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source = gl_from_color(context, uv);
        let destination = gl_to_color(context, uv);
        builtin_valid([
            source[0] * from_m + destination[0] * to_m,
            source[1] * from_m + destination[1] * to_m,
            source[2] * from_m + destination[2] * to_m,
            mix(source[3], destination[3], progress),
        ])
    });
}

/// `advanced-mosaic.wgsl`.
fn advanced_mosaic(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let t = context.progress();
    let pixel_size = context.float("pixel_size");
    let size = if t < 0.5 {
        mix(1.0, pixel_size, t / 0.5)
    } else {
        mix(pixel_size, 1.0, (t - 0.5) / 0.5)
    };
    let d = size * 0.005;
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let coord = uv.map(|value| (d * ((value - 0.5) / d - 0.5).ceil() + 0.5).clamp(0.0, 1.0));
        mix4(
            gl_from_color(context, coord),
            gl_to_color(context, coord),
            t,
        )
    });
}

/// `burn.wgsl`.
fn burn(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let glow = builtin_premultiply(context.vec4("color"));
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source = gl_from_color(context, uv);
        let destination = gl_to_color(context, uv);
        let mut color = [0.0; 4];
        for channel in 0..3 {
            let lit_from = source[channel] + progress * glow[channel] * source[3];
            let lit_to = destination[channel] + (1.0 - progress) * glow[channel] * destination[3];
            color[channel] = mix(lit_from, lit_to, progress);
        }
        color[3] = mix(source[3], destination[3], progress);
        builtin_valid(color)
    });
}

/// `crosshatch.wgsl`.
fn crosshatch(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let center = context.vec2("center");
    let threshold = context.float("threshold");
    let fade_edge = context.float("fade_edge");
    let resolution = context.resolution();
    let fade_out = builtin_smoothstep(1.0 - fade_edge, 1.0, progress);
    let fade_in = builtin_smoothstep(0.0, fade_edge, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let pixel = [
            (uv[0] * resolution[0]).floor().max(0.0) as u32,
            (uv[1] * resolution[1]).floor().max(0.0) as u32,
        ];
        let row = builtin_unit(builtin_hash3(pixel[1], 0, 0));
        let column = builtin_unit(builtin_hash3(pixel[0], 1, 0));
        let dist = distance(center, uv) / threshold;
        let r = progress - row.min(column);
        let amount = mix(0.0, mix(step(dist, r), 1.0, fade_out), fade_in);
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), amount)
    });
}

/// `butterfly_acos`: Abramowitz and Stegun 4.4.46.
#[allow(clippy::excessive_precision, clippy::unreadable_literal)]
fn butterfly_acos(value: f32) -> f32 {
    let x = value.clamp(-1.0, 1.0).abs();
    let polynomial =
        ((((((-0.0012624911 * x + 0.0066700901) * x - 0.0170881256) * x + 0.0308918810) * x
            - 0.0501743046)
            * x
            + 0.0889789874)
            * x
            - 0.2145988016)
            * x
            + 1.5707963050;
    let angle = (1.0 - x).sqrt() * polynomial;
    select(angle, PI - angle, value < 0.0)
}

/// `butterfly-wave-scrawler.wgsl`.
fn butterfly_wave_scrawler(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let amplitude = context.float("amplitude");
    let waves = context.float("waves");
    let color_separation = context.float("color_separation");
    let inv = 1.0 - progress;
    let reach = sinf(progress * amplitude);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let o = sub(scale(uv, reach), [0.5, 0.5]);
        let theta = butterfly_acos(o[0]) * waves;
        let s = sinf((2.0 * theta - PI) / 24.0);
        let s2 = s * s;
        let disp = (expf(cosf(theta)) - 2.0 * cosf(4.0 * theta) + s2 * s2 * s) / 10.0;
        let shift = |amount: f32| [uv[0] + amount, uv[1] + amount];
        let tex_to = gl_to_color(context, shift(inv * disp));
        let red = gl_from_color(context, shift(progress * disp * (1.0 - color_separation)));
        let green = gl_from_color(context, shift(progress * disp));
        let blue = gl_from_color(context, shift(progress * disp * (1.0 + color_separation)));
        let tex_from = [red[0], green[1], blue[2], red[3].max(green[3].max(blue[3]))];
        add4(
            tex_to.map(|channel| channel * progress),
            tex_from.map(|channel| channel * inv),
        )
    });
}

/// `crazy-parametric-fun.wgsl`.
#[allow(clippy::many_single_char_names)]
fn crazy_parametric_fun(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let a = context.float("a");
    let b = context.float("b");
    let amplitude = context.float("amplitude");
    let smoothness = context.float("smoothness");
    let x = (a - b) * polynomial_sin(progress + FRAC_PI_2)
        + b * polynomial_sin(progress * ((a / b) - 1.0) + FRAC_PI_2);
    let y_curve =
        (a - b) * polynomial_sin(progress) - b * polynomial_sin(progress * ((a / b) - 1.0));
    let amount = builtin_smoothstep(0.2, 1.0, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let dir = sub(uv, [0.5, 0.5]);
        let dist = length(dir);
        let offset = [
            dir[0] * polynomial_sin(progress * dist * amplitude * x) / smoothness,
            dir[1] * polynomial_sin(progress * dist * amplitude * y_curve) / smoothness,
        ];
        mix4(
            gl_from_color(context, add(uv, offset)),
            gl_to_color(context, uv),
            amount,
        )
    });
}

/// `edge`'s 3 x 3 taps' edge strength times the centre tap, tap `3 * i + j`
/// at offset `(i - 1, j - 1)`; alpha is the centre tap's.
fn edge_color(c: &[[f32; 3]; 9], centre: Rgba, brightness: f32) -> Rgba {
    let mut sum = [0.0; 3];
    for (channel, value) in sum.iter_mut().enumerate() {
        let dx = 2.0 * (c[7][channel] - c[1][channel]).abs()
            + (c[2][channel] - c[6][channel]).abs()
            + (c[8][channel] - c[0][channel]).abs();
        let dy = 2.0 * (c[3][channel] - c[5][channel]).abs()
            + (c[6][channel] - c[8][channel]).abs()
            + (c[0][channel] - c[2][channel]).abs();
        *value = 0.25 * (dx + dy) * 0.5;
    }
    let delta = (sum[0] * sum[0] + sum[1] * sum[1] + sum[2] * sum[2]).sqrt();
    let k = (brightness * delta).clamp(0.0, 1.0);
    [k * centre[0], k * centre[1], k * centre[2], centre[3]]
}

fn edge(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let thickness = context.float("edge_thickness");
    let brightness = context.float("edge_brightness");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut from_taps = [[0.0; 3]; 9];
        let mut to_taps = [[0.0; 3]; 9];
        for i in 0..3_u8 {
            for j in 0..3_u8 {
                let p = [
                    uv[0] + thickness * (f32::from(i) - 1.0),
                    uv[1] + thickness * (f32::from(j) - 1.0),
                ];
                let index = usize::from(3 * i + j);
                let from = gl_from_color(context, p);
                let to = gl_to_color(context, p);
                from_taps[index] = [from[0], from[1], from[2]];
                to_taps[index] = [to[0], to[1], to[2]];
            }
        }
        let source = gl_from_color(context, uv);
        let target = gl_to_color(context, uv);
        let start = mix4(
            source,
            edge_color(&from_taps, source, brightness),
            (2.0 * progress).clamp(0.0, 1.0),
        );
        let end = mix4(
            edge_color(&to_taps, target, brightness),
            target,
            (2.0 * (progress - 0.5)).clamp(0.0, 1.0),
        );
        mix4(start, end, progress)
    });
}

fn fly_eye(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let size = context.float("size");
    let zoom = context.float("zoom");
    let separation = context.float("color_separation");
    let inv = 1.0 - progress;
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let displacement = [size * cosf(zoom * uv[0]), size * sinf(zoom * uv[1])];
        let to = gl_to_color(context, add(uv, scale(displacement, inv)));
        let moved = scale(displacement, progress);
        let red = gl_from_color(context, add(uv, scale(moved, 1.0 - separation)));
        let green = gl_from_color(context, add(uv, moved));
        let blue = gl_from_color(context, add(uv, scale(moved, 1.0 + separation)));
        let from = [red[0], green[1], blue[2], red[3].max(green[3].max(blue[3]))];
        [0, 1, 2, 3].map(|channel| to[channel] * progress + from[channel] * inv)
    });
}

/// `hexagonalize`'s nearest hexagon, axial `(q, r)`, of a fractional one.
#[allow(clippy::float_cmp)]
fn round_hexagon(q: f32, r: f32) -> Vec2 {
    let s = -q - r;
    let mut rounded_q = (q + 0.5).floor();
    let mut rounded_r = (r + 0.5).floor();
    let rounded_s = (s + 0.5).floor();
    let delta_q = (rounded_q - q).abs();
    let delta_r = (rounded_r - r).abs();
    let delta_s = (rounded_s - s).abs();
    if delta_q > delta_r && delta_q > delta_s {
        rounded_q = -rounded_r - rounded_s;
    } else if delta_r > delta_s {
        rounded_r = -rounded_q - rounded_s;
    }
    [rounded_q, rounded_r]
}

fn hexagonalize(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let steps = context.float("steps");
    let ratio = context.ratio();
    let mut dist = 2.0 * progress.min(1.0 - progress);
    if steps > 0.0 {
        dist = (dist * steps).ceil() / steps;
    }
    let size = 0.577_350_26 * dist / context.float("horizontal_hexagons");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut point = uv;
        if dist > 0.0 {
            let centered = [(uv[0] - 0.5) / size, (uv[1] / ratio - 0.5) / size];
            let hexagon = round_hexagon(
                0.577_350_26 * centered[0] + -0.333_333_34 * centered[1],
                0.666_666_7 * centered[1],
            );
            let x = (1.732_050_8 * hexagon[0] + 0.866_025_4 * hexagon[1]) * size + 0.5;
            let y = (1.5 * hexagon[1]) * size + 0.5;
            point = [x, y * ratio];
        }
        mix4(
            gl_from_color(context, point),
            gl_to_color(context, point),
            progress,
        )
    });
}

fn kaleidoscope(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let speed = context.float("speed");
    let angle = context.float("angle");
    let power = context.float("power");
    let amount = 1.0 - 2.0 * (progress - 0.5).abs();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut p = sub(uv, [0.5, 0.5]);
        let mut t = powf(progress, power) * speed;
        for _ in 0..7 {
            let (sine, cosine) = (sinf(t), cosf(t));
            p = [sine * p[0] + cosine * p[1], sine * p[1] - cosine * p[0]];
            t += angle;
            p = p.map(|v| (builtin_floor_mod(v, 2.0) - 1.0).abs());
        }
        mix4(
            mix4(
                gl_from_color(context, uv),
                gl_to_color(context, uv),
                progress,
            ),
            mix4(gl_from_color(context, p), gl_to_color(context, p), progress),
            amount,
        )
    });
}

fn luma_burn(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let line_width = context.float("line_width");
    let spread = context.vec4("spread_color");
    let hot = context.vec4("hot_color");
    let power = context.float("power");
    let intensity = context.float("intensity");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let source = gl_from_color(context, uv);
        let target = gl_to_color(context, uv);
        let straight = builtin_straight(source);
        let burn = 0.5 + 0.5 * (0.299 * straight[0] + 0.587 * straight[1] + 0.114 * straight[2]);
        let show = burn - progress;
        if show < 0.001 {
            return target;
        }
        let factor = 1.0 - builtin_smoothstep(0.0, line_width, show);
        let mut burned = [0.0, 0.0, 0.0, source[3]];
        for channel in 0..3 {
            let burn_color = (powf(mix(spread[channel], hot[channel], factor), power) * intensity)
                .clamp(0.0, 1.0);
            burned[channel] = mix(straight[channel], burn_color, factor) * source[3];
        }
        burned
    });
}

fn luminance_melt(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let center = [1.0, context.choice("direction") as f32];
    let threshold = context.float("l_threshold");
    let above = context.boolean("above");
    let width = context.resolution()[0];
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let noise = 2.0 * builtin_value_noise([uv[0], 0.0], 0) - 1.0;
        let dist = distance(center, uv) - progress * expf(noise);
        let column = builtin_unit(builtin_hash(builtin_cell(uv[0] * width)));
        let r = progress - column;
        let source = gl_from_color(context, uv);
        let straight = builtin_straight(source);
        let luminance = straight[0] * 0.299 + straight[1] * 0.587 + straight[2] * 0.114;
        let bright = if above {
            luminance > threshold
        } else {
            luminance < threshold
        };
        let m = if dist <= r && bright {
            1.0
        } else {
            progress * progress * progress
        };
        mix4(source, gl_to_color(context, uv), m)
    });
}

/// `power-kaleido`'s `rotate_about`.
fn rotate_about(p: Vec2, origin: Vec2, angle: f32) -> Vec2 {
    let s = sinf(angle);
    let c = cosf(angle);
    let d = sub(p, origin);
    add(origin, [c * d[0] + s * d[1], -s * d[0] + c * d[1]])
}

/// `power-kaleido`'s mirrors at 0, 120 and 240 degrees: cos, sin, tan.
const POWER_KALEIDO_MIRRORS: [[f32; 3]; 3] = [
    [1.0, 0.0, 0.0],
    [-0.5, 0.866_025_4, -1.732_050_8],
    [-0.5, -0.866_025_4, 1.732_050_8],
];

fn power_kaleido(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let dist = context.float("scale") / 10.0;
    let zoom = context.float("z");
    let speed = context.float("speed");
    let ratio = context.ratio();
    let uv_amount = cosf(progress * TAU) / 2.0 + 0.5;
    let amount = cosf((progress - 1.0) * PI) / 2.0 + 0.5;
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut p = sub(uv, [0.5, 0.5]);
        p[0] *= ratio;
        p = scale(p, zoom);
        p = rotate_about(p, [0.0, 0.0], progress * speed);
        for _ in 0..10 {
            for mirror in POWER_KALEIDO_MIRRORS {
                let lhs = p[1] - dist * mirror[0];
                let rhs = mirror[2] * (p[0] + dist * mirror[1]);
                if (mirror[0] > 0.0 && lhs > rhs) || (mirror[0] <= 0.0 && lhs < rhs) {
                    let q = [p[0] + mirror[1] * dist * 2.0, p[1] - mirror[0] * dist * 2.0];
                    let n = [mirror[0], mirror[1]];
                    let projection = dot(q, n);
                    p = [
                        2.0 * n[0] * projection - q[0],
                        2.0 * n[1] * projection - q[1],
                    ];
                }
            }
        }
        p = add(p, [0.5, 0.5]);
        p = rotate_about(p, [0.5, 0.5], progress * -speed);
        p = sub(p, [0.5, 0.5]);
        p[0] /= ratio;
        p = add(p, [0.5, 0.5]);
        p = p.map(|v| 2.0 * (v / 2.0 - (v / 2.0 + 0.5).floor()).abs());
        let uv_mix = [mix(p[0], uv[0], uv_amount), mix(p[1], uv[1], uv_amount)];
        mix4(
            gl_from_color(context, uv_mix),
            gl_to_color(context, uv_mix),
            amount,
        )
    });
}

fn random_noise(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let resolution = context.resolution();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let pixel = [uv[0] * resolution[0], uv[1] * resolution[1]];
        let random = builtin_unit(builtin_hash3(
            builtin_cell(pixel[0]),
            builtin_cell(pixel[1]),
            0,
        ));
        let amount = (random + progress).floor();
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), amount)
    });
}

fn swirl(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let t = context.progress();
    let amount = if t <= 0.5 {
        t / 0.5
    } else {
        1.0 - (t - 0.5) / 0.5
    };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut p = sub(uv, [0.5, 0.5]);
        let dist = length(p);
        if dist < 1.0 {
            let percent = 1.0 - dist;
            // Upstream's pi, to five decimals.
            #[allow(clippy::approx_constant)]
            let theta = percent * percent * amount * 8.0 * 3.14159;
            let sine = sinf(theta);
            let cosine = cosf(theta);
            p = [dot(p, [cosine, -sine]), dot(p, [sine, cosine])];
        }
        p = add(p, [0.5, 0.5]);
        mix4(gl_from_color(context, p), gl_to_color(context, p), t)
    });
}

/// `undulating-burn-out`'s `quadratic_in_out`.
fn quadratic_in_out(t: f32) -> f32 {
    let p = 2.0 * t * t;
    if t < 0.5 { p } else { -p + 4.0 * t - 1.0 }
}

fn undulating_burn_out(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let smoothness = context.float("smoothness");
    let center = context.vec2("center");
    let color = builtin_premultiply(context.vec4("color"));
    let eased = builtin_smoothstep(0.0, 1.0, progress);
    let magnitude = mix(0.02, 0.09, eased);
    let offset = mix(40.0, 30.0, eased);
    let wave = |p: Vec2| {
        let offset_from_center = sub(p, center);
        let degs = (atan2f(offset_from_center[1], offset_from_center[0]) * 57.295_78 + 180.0)
            * 0.261_799_4;
        progress + quadratic_in_out(sinf(degs)) * magnitude * sinf(progress * offset)
    };
    // The distance inside the ring.
    let inside = |p: Vec2| wave(p) - distance(center, p);
    transition_row(context, y, x0, out, |x, uv| {
        let step_at = |edge: f32| {
            let width = fwidth_fine(context.size, x, y, |uv| inside(gl_uv(uv)) - edge);
            builtin_aa_step(edge, inside(gl_uv(uv)), width)
        };
        let past_glow = step_at(-0.005);
        let past_burn = step_at(0.005);
        let past_band = step_at(0.01);
        let uv = gl_uv(uv);
        let r = wave(uv);
        let dist = distance(center, uv);
        let source = gl_from_color(context, uv);
        let target = gl_to_color(context, uv);
        let smooth = mix4(
            source,
            target,
            builtin_smoothstep(-smoothness, 0.0, r - dist * (1.0 + smoothness)),
        );
        let glow = builtin_valid(mix4(source, target, -1.0));
        let burn = mix4(source, color, 0.75);
        let smooth_weight = 1.0 - past_glow + past_band;
        let glow_weight = past_glow - past_burn;
        let burn_weight = past_burn - past_band;
        [0, 1, 2, 3].map(|channel| {
            smooth[channel] * smooth_weight
                + glow[channel] * glow_weight
                + burn[channel] * burn_weight
        })
    });
}

fn water_drop(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let amplitude = context.float("amplitude");
    let speed = context.float("speed");
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| {
            length(sub(gl_uv(uv), [0.5, 0.5])) - progress
        });
        let uv = gl_uv(uv);
        let direction = sub(uv, [0.5, 0.5]);
        let dist = length(direction);
        let outside = builtin_aa_step(progress, dist, width);
        let offset = scale(direction, sinf(dist * amplitude - progress * speed));
        let target = gl_to_color(context, uv);
        let still = mix4(gl_from_color(context, uv), target, progress);
        let rippled = mix4(gl_from_color(context, add(uv, offset)), target, progress);
        mix4(rippled, still, outside)
    });
}
