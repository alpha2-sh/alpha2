//! The built-in transitions (builtins.rs `builtin_transition_definitions`).
//!
//! Each kernel is a literal port of its shader body in
//! `alpha2-shaders/shaders/runtime/transitions`: the same operations in the
//! same order, in gl-transitions' y-up coordinates (`gl_uv`). A
//! `fwidthFine` is the same expression evaluated at the pixel's 2 x 2 quad
//! partners, written as a function of the shader's `uv`.

// The shaders' own conversions between f32, u32 and the pixel grid.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use core::f32::consts::{PI, SQRT_2};

use libm::{atan2f, cosf, powf, sinf};

use super::{
    CpuKernel, KernelContext, builtin_aa_pulse, builtin_aa_ramp, builtin_aa_smoothstep,
    builtin_aa_step, builtin_angle_width, builtin_interval_coverage, builtin_premultiply,
    builtin_smoothstep, builtin_smoothstep4, builtin_straight, fract, fwidth_fine, gl_from_color,
    gl_to_color, gl_uv, mix, mix4, pcg, smoothstep, step, transition_row,
};

type Vec2 = [f32; 2];
type Rgba = [f32; 4];
type Row = fn(&KernelContext<'_>, u32, u32, &mut [Rgba]);

/// A transition kernel: its row function.
struct Transition(Row);

impl CpuKernel for Transition {
    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
        (self.0)(context, y, x0, out);
    }
}

static KERNELS: [(&str, Transition); 30] = [
    ("fade", Transition(fade)),
    ("directional", Transition(directional)),
    ("circle-open", Transition(circle_open)),
    ("angular", Transition(angular)),
    ("pixelize", Transition(pixelize)),
    ("directional-wipe", Transition(directional_wipe)),
    ("circle", Transition(circle)),
    ("cross-warp", Transition(cross_warp)),
    ("dreamy", Transition(dreamy)),
    ("radial", Transition(radial)),
    ("wipe-up", Transition(wipe_up)),
    ("wipe-down", Transition(wipe_down)),
    ("wipe-left", Transition(wipe_left)),
    ("wipe-right", Transition(wipe_right)),
    ("window-blinds", Transition(window_blinds)),
    ("window-slice", Transition(window_slice)),
    ("wind", Transition(wind)),
    ("pinwheel", Transition(pinwheel)),
    ("heart", Transition(heart)),
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
];

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    let name = id.strip_prefix("alpha2.transition.")?;
    KERNELS
        .iter()
        .find(|(kernel, _)| *kernel == name)
        .map(|(_, kernel)| kernel as &dyn CpuKernel)
}

#[inline]
fn add(a: Vec2, b: Vec2) -> Vec2 {
    [a[0] + b[0], a[1] + b[1]]
}

#[inline]
fn sub(a: Vec2, b: Vec2) -> Vec2 {
    [a[0] - b[0], a[1] - b[1]]
}

#[inline]
fn scale(a: Vec2, s: f32) -> Vec2 {
    [a[0] * s, a[1] * s]
}

#[inline]
fn dot(a: Vec2, b: Vec2) -> f32 {
    a[0] * b[0] + a[1] * b[1]
}

#[inline]
fn length(a: Vec2) -> f32 {
    dot(a, a).sqrt()
}

#[inline]
fn distance(a: Vec2, b: Vec2) -> f32 {
    length(sub(a, b))
}

#[inline]
fn normalize(a: Vec2) -> Vec2 {
    let length = length(a);
    [a[0] / length, a[1] / length]
}

/// WGSL `sign`: zero for zero, unlike `f32::signum`.
#[inline]
fn sign(x: f32) -> f32 {
    if x > 0.0 {
        1.0
    } else if x < 0.0 {
        -1.0
    } else {
        0.0
    }
}

#[inline]
fn add4(a: Rgba, b: Rgba) -> Rgba {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]]
}

#[inline]
const fn select(rejected: f32, accepted: f32, condition: bool) -> f32 {
    if condition { accepted } else { rejected }
}

/// `normalize(direction)` scaled to unit L1 length, for the directional
/// wipe and warp; `fallback` for a direction of (almost) zero length.
fn l1_direction(direction: Vec2, fallback: Vec2) -> Vec2 {
    let direction = if length(direction) < 0.000_001 {
        fallback
    } else {
        direction
    };
    let vector = normalize(direction);
    let l1 = vector[0].abs() + vector[1].abs();
    [vector[0] / l1, vector[1] / l1]
}

fn fade(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            progress,
        )
    });
}

fn directional(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let direction = context.vec2("direction");
    // The schema admits (0, 0), which moves neither clip: the default instead.
    let mut direction_step = [sign(direction[0]), sign(direction[1])];
    if direction_step == [0.0, 0.0] {
        direction_step = [0.0, 1.0];
    }
    let resolution = context.resolution();
    let width = [1.0 / resolution[0], 1.0 / resolution[1]];
    let side = context.max_filter_samples().clamp(1.0, 16.0).sqrt().floor();
    let sample = |uv: Vec2| {
        let p = add(uv, scale(direction_step, progress));
        let wrapped = [fract(p[0]), fract(p[1])];
        let inside = step(0.0, p[1]) * step(p[1], 1.0) * step(0.0, p[0]) * step(p[0], 1.0);
        mix4(
            gl_to_color(context, wrapped),
            gl_from_color(context, wrapped),
            inside,
        )
    };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let p = add(uv, scale(direction_step, progress));
        let coverage =
            builtin_interval_coverage(p[0], width[0]) * builtin_interval_coverage(p[1], width[1]);
        if coverage <= 0.0 || coverage >= 1.0 {
            return sample(uv);
        }
        // The two images wrap at the seam: integrate subpixel samples there.
        let mut color = [0.0; 4];
        for row in 0..side as u32 {
            for column in 0..side as u32 {
                let offset = [
                    ((column as f32 + 0.5) / side - 0.5) * width[0],
                    ((row as f32 + 0.5) / side - 0.5) * width[1],
                ];
                color = add4(color, sample(add(uv, offset)));
            }
        }
        let count = side * side;
        color.map(|channel| channel / count)
    });
}

fn circle_open(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let smoothness = context.float("smoothness");
    let opening = context.boolean("opening");
    let phase = select(1.0 - progress, progress, opening);
    let value = |uv: Vec2| SQRT_2 * distance([0.5, 0.5], uv) - phase * (1.0 + smoothness);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| value(gl_uv(uv)));
        let uv = gl_uv(uv);
        let mask = builtin_aa_smoothstep(-smoothness, 0.0, value(uv), width);
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            select(mask, 1.0 - mask, opening),
        )
    });
}

fn angular(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let offset = context.float("starting_angle") * PI / 180.0;
    let resolution = context.resolution();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let angle = atan2f(uv[1] - 0.5, uv[0] - 0.5) + offset;
        let normalized_angle = fract((angle + PI) / (2.0 * PI));
        let width = builtin_angle_width(sub(uv, [0.5, 0.5]), resolution) / (2.0 * PI);
        let mask = builtin_aa_pulse(normalized_angle, progress, width);
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

fn pixelize(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let steps = context.float("steps");
    let squares_min = context.vec2("squares_min");
    let distance_to_end = progress.min(1.0 - progress);
    let mut distance_step = distance_to_end;
    if steps > 0.0 {
        distance_step = (distance_to_end * steps).ceil() / steps;
    }
    let square_size = [
        2.0 * distance_step / squares_min[0].max(1.0),
        2.0 * distance_step / squares_min[1].max(1.0),
    ];
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut sample_uv = uv;
        if distance_step > 0.0 {
            sample_uv = [
                ((uv[0] / square_size[0]).floor() + 0.5) * square_size[0],
                ((uv[1] / square_size[1]).floor() + 0.5) * square_size[1],
            ];
        }
        mix4(
            gl_from_color(context, sample_uv),
            gl_to_color(context, sample_uv),
            progress,
        )
    });
}

fn directional_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let smoothness = context.float("smoothness");
    let vector = l1_direction(context.vec2("direction"), [1.0, -1.0]);
    let edge = dot(vector, [0.5, 0.5]);
    let value = |uv: Vec2| dot(vector, uv) - (edge - 0.5 + progress * (1.0 + smoothness));
    let started = 1.0 - step(progress, 0.0);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| value(gl_uv(uv)));
        let uv = gl_uv(uv);
        let mask = started * (1.0 - builtin_aa_smoothstep(-smoothness, 0.0, value(uv), width));
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

fn circle(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let center = context.vec2("center");
    let back_color = builtin_premultiply(context.vec4("back_color"));
    let radius = 8.0_f32.sqrt() * (progress - 0.5).abs();
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| {
            distance(gl_uv(uv), center) - radius
        });
        let uv = gl_uv(uv);
        let coverage = (1.0 - builtin_aa_step(radius, distance(uv, center), width))
            * select(0.0, 1.0, radius > 0.0);
        let image = if progress >= 0.5 {
            gl_to_color(context, uv)
        } else {
            gl_from_color(context, uv)
        };
        mix4(back_color, image, coverage)
    });
}

fn cross_warp(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let phase = smoothstep(0.0, 1.0, progress * 2.0 + uv[0] - 1.0);
        let from_uv = add(scale(sub(uv, [0.5, 0.5]), 1.0 - phase), [0.5, 0.5]);
        let to_uv = add(scale(sub(uv, [0.5, 0.5]), phase), [0.5, 0.5]);
        mix4(
            gl_from_color(context, from_uv),
            gl_to_color(context, to_uv),
            phase,
        )
    });
}

fn dreamy_offset(phase: f32, x: f32) -> Vec2 {
    let shift_y = 0.03 * phase * cosf(10.0 * (phase + x));
    [0.0, shift_y]
}

fn dreamy(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        mix4(
            gl_from_color(context, add(uv, dreamy_offset(progress, uv[0]))),
            gl_to_color(context, add(uv, dreamy_offset(1.0 - progress, uv[0]))),
            progress,
        )
    });
}

fn radial(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let smoothness = context.float("smoothness");
    let resolution = context.resolution();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let centered = [uv[0] * 2.0 - 1.0, uv[1] * 2.0 - 1.0];
        let edge = atan2f(centered[1], centered[0]) - (progress - 0.5) * PI * 2.5;
        mix4(
            gl_to_color(context, uv),
            gl_from_color(context, uv),
            builtin_aa_ramp(
                0.0,
                smoothness,
                edge,
                builtin_angle_width(sub(uv, [0.5, 0.5]), resolution),
            ),
        )
    });
}

/// The four wipes: `edge(uv)` against progress, antialiased over the
/// pixel's footprint.
fn wipe(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [Rgba],
    edge: impl Fn(Vec2) -> f32,
) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| progress - edge(gl_uv(uv)));
        let uv = gl_uv(uv);
        let mask = builtin_aa_step(edge(uv), progress, width);
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

fn wipe_up(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    wipe(context, y, x0, out, |uv| uv[1]);
}

fn wipe_down(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    wipe(context, y, x0, out, |uv| 1.0 - uv[1]);
}

fn wipe_left(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    wipe(context, y, x0, out, |uv| 1.0 - uv[0]);
}

fn wipe_right(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    wipe(context, y, x0, out, |uv| uv[0]);
}

fn window_blinds(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let stripe = |uv: Vec2| uv[1] * 50.0 * progress;
    let settle = smoothstep(0.8, 1.0, progress);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| stripe(gl_uv(uv)));
        let uv = gl_uv(uv);
        let coverage = builtin_aa_pulse(stripe(uv), 0.5, width);
        let phase = progress * (1.0 + 0.5 * coverage);
        let amount = mix(phase, progress, settle);
        // `amount` passes 1 while the stripes run ahead: clamp to the
        // premultiplied range.
        let mixed = mix4(gl_from_color(context, uv), gl_to_color(context, uv), amount);
        let alpha = mixed[3].clamp(0.0, 1.0);
        [
            mixed[0].max(0.0).min(alpha),
            mixed[1].max(0.0).min(alpha),
            mixed[2].max(0.0).min(alpha),
            alpha,
        ]
    });
}

fn window_slice(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let slices = context.float("count");
    let smoothness = context.float("smoothness");
    let resolution = context.resolution();
    let mask_at = |x: f32| {
        let phase = builtin_smoothstep(-smoothness, 0.0, x - progress * (1.0 + smoothness));
        step(phase, fract(slices * x))
    };
    // Only the mask is integrated, keeping image details at pixel centres.
    let count = context.max_filter_samples().clamp(1.0, 16.0);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut mask = 0.0;
        for i in 0..count as u32 {
            let offset = ((i as f32 + 0.5) / count - 0.5) / resolution[0];
            mask += mask_at(uv[0] + offset);
        }
        mask /= count;
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

fn wind(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let size = context.float("size");
    let reversed = context.boolean("reversed");
    let resolution = context.resolution();
    // One value in [0, 1) per output row; the top 24 bits convert exactly.
    let random = |uv_y: f32| {
        let row = (uv_y * resolution[1]).floor().max(0.0) as u32;
        (pcg(row) >> 8) as f32 / 16_777_216.0
    };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let x = select(uv[0], 1.0 - uv[0], reversed);
        let mask = builtin_smoothstep(
            0.0,
            -size,
            x * (1.0 - size) + size * random(uv[1]) - progress * (1.0 + size),
        );
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

fn pinwheel(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let speed = context.float("speed");
    let resolution = context.resolution();
    // The original's own constant, not pi.
    #[allow(clippy::approx_constant)]
    let period = 3.1415_f32 / 4.0;
    let duty = (progress / period).clamp(0.0, 1.0);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let circle_position = atan2f(uv[1] - 0.5, uv[0] - 0.5) + progress * speed;
        let mask = builtin_aa_pulse(
            circle_position / period,
            duty,
            builtin_angle_width(sub(uv, [0.5, 0.5]), resolution) / period,
        );
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

/// `in_heart`'s edge and value at `point`.
fn heart_terms(point: Vec2, center: Vec2, size: f32) -> (f32, f32) {
    let divisor = 1.6 * size;
    let offset = [
        (point[0] - center[0]) / divisor,
        (point[1] - center[1]) / divisor,
    ];
    let curve = offset[0] * offset[0] + offset[1] * offset[1] - 0.3;
    (
        curve * curve * curve,
        offset[0] * offset[0] * offset[1] * offset[1] * offset[1],
    )
}

fn heart(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let center = [0.5, 0.4];
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| {
            let (edge, value) = heart_terms(gl_uv(uv), center, progress);
            value - edge
        });
        let uv = gl_uv(uv);
        let mask = if progress == 0.0 {
            0.0
        } else {
            let (edge, value) = heart_terms(uv, center, progress);
            builtin_aa_step(edge, value, width)
        };
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), mask)
    });
}

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
