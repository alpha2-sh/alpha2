//! The "Classic" group's kernels.

use super::{
    KernelContext, PI, Rgba, SQRT_2, TAU, Transition, Vec2, add, add4, atan2f, builtin_aa_pulse,
    builtin_aa_ramp, builtin_aa_smoothstep, builtin_aa_step, builtin_angle_width,
    builtin_interval_coverage, builtin_premultiply, builtin_smoothstep, builtin_straight, cosf,
    distance, dot, fract, fwidth_fine, gl_from_color, gl_to_color, gl_uv, l1_direction, length,
    mix4, polynomial_sin, scale, select, sign, sinf, smoothstep, step, sub, transition_row,
};
use crate::cpu::kernels::{builtin_hsv_to_rgb, builtin_over, builtin_rgb_to_hsv, mix};

pub(super) static KERNELS: &[(&str, Transition)] = &[
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
    ("bounce", Transition(bounce)),
    ("directional-scaled", Transition(directional_scaled)),
    ("slides", Transition(slides)),
    ("fold", Transition(fold)),
    ("directional-easing", Transition(directional_easing)),
    ("hsv-fade", Transition(hsv_fade)),
    ("left-right", Transition(left_right)),
    ("top-bottom", Transition(top_bottom)),
    ("multiply-blend", Transition(multiply_blend)),
    ("rolls", Transition(rolls)),
    (
        "split-slide-in-horizontal",
        Transition(split_slide_in_horizontal),
    ),
    (
        "split-slide-in-vertical",
        Transition(split_slide_in_vertical),
    ),
    (
        "split-slide-out-horizontal",
        Transition(split_slide_out_horizontal),
    ),
    (
        "split-slide-out-vertical",
        Transition(split_slide_out_vertical),
    ),
    (
        "split-slide-in-out-horizontal",
        Transition(split_slide_in_out_horizontal),
    ),
    (
        "split-slide-in-out-vertical",
        Transition(split_slide_in_out_vertical),
    ),
    ("rotate", Transition(rotate)),
    ("x-axis-translation", Transition(x_axis_translation)),
    ("cover", Transition(cover)),
    ("uncover", Transition(uncover)),
];

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

/// `bounce.wgsl`.
fn bounce(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let shadow_color = context.vec4("shadow_color");
    let shadow_height = context.float("shadow_height");
    let bounces = context.float("bounces");
    let eased = sinf(progress * PI / 2.0);
    let phase = progress * PI * bounces;
    let height = cosf(phase).abs() * (1.0 - eased);
    let shadow_alpha = shadow_color[3];
    let fade_out = builtin_smoothstep(0.95, 1.0, progress);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| 0.0 - (gl_uv(uv)[1] - height));
        let uv = gl_uv(uv);
        let d = uv[1] - height;
        let dropped = builtin_aa_step(d, 0.0, width);
        let fade = mix(
            (d / shadow_height) * shadow_alpha + (1.0 - shadow_alpha),
            1.0,
            fade_out,
        );
        let shadow = (step(d, shadow_height) * (1.0 - fade)).clamp(0.0, 1.0);
        let top = [
            shadow_color[0] * shadow,
            shadow_color[1] * shadow,
            shadow_color[2] * shadow,
            shadow,
        ];
        let shaded = builtin_over(top, gl_to_color(context, uv));
        mix4(
            shaded,
            gl_from_color(context, [uv[0], uv[1] + (1.0 - height)]),
            dropped,
        )
    });
}

/// The direction `directional-scaled` and `directional-easing` push in:
/// the signs of `direction`, (0, 1) for (0, 0).
fn push_heading(context: &KernelContext<'_>) -> Vec2 {
    let direction = context.vec2("direction");
    let heading = [sign(direction[0]), sign(direction[1])];
    if heading == [0.0, 0.0] {
        [0.0, 1.0]
    } else {
        heading
    }
}

/// The directional pushes' pixel: one tap off the wrap seam, `max_side`
/// squared subpixel taps within half a pixel of it along an axis `heading`
/// moves on; `tap` takes the tap and that side.
fn push_pixel(
    width: Vec2,
    max_side: f32,
    uv: Vec2,
    (heading, shift): (Vec2, Vec2),
    tap: impl Fn(Vec2, f32) -> Rgba,
) -> Rgba {
    let p = add(uv, shift);
    let near = |axis: usize| {
        p[axis].abs().min((p[axis] - 1.0).abs()) < width[axis] * 0.5 && heading[axis] != 0.0
    };
    let side = if near(0) || near(1) { max_side } else { 1.0 };
    let mut color = [0.0; 4];
    for row in 0..side as u32 {
        for column in 0..side as u32 {
            let offset = [
                ((column as f32 + 0.5) / side - 0.5) * width[0],
                ((row as f32 + 0.5) / side - 0.5) * width[1],
            ];
            color = add4(color, tap(add(add(uv, offset), shift), side));
        }
    }
    let count = side * side;
    color.map(|channel| channel / count)
}

/// `directional-scaled.wgsl`.
fn directional_scaled(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let heading = push_heading(context);
    let wave = polynomial_sin(progress * PI / 2.0);
    let eased = wave * wave * wave;
    let s = 1.0 - (1.0 - (1.0 / context.float("scale"))) * polynomial_sin(progress * PI);
    let resolution = context.resolution();
    let width = [1.0 / resolution[0], 1.0 / resolution[1]];
    let max_side = context.max_filter_samples().clamp(1.0, 16.0).sqrt().floor();
    let shift = scale(heading, eased);
    // Each tap covers its share of the pixel.
    let tap = |q: Vec2, side: f32| {
        let border_span = [
            (width[0] * s / side).max(0.000_000_1),
            (width[1] * s / side).max(0.000_000_1),
        ];
        let f = [(fract(q[0]) - 0.5) * s + 0.5, (fract(q[1]) - 0.5) * s + 0.5];
        let inside = step(0.0, q[1]) * step(q[1], 1.0) * step(0.0, q[0]) * step(q[0], 1.0);
        let color = mix4(gl_to_color(context, f), gl_from_color(context, f), inside);
        let border = builtin_interval_coverage(f[0], border_span[0])
            * builtin_interval_coverage(f[1], border_span[1]);
        color.map(|channel| channel * border)
    };
    transition_row(context, y, x0, out, |_, uv| {
        push_pixel(width, max_side, gl_uv(uv), (heading, shift), tap)
    });
}

/// `slides.wgsl`.
fn slides(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let slide_in = context.boolean("slide_in");
    let rad = select(1.0 - progress, progress, slide_in);
    let half = rad / 2.0;
    // The sliding picture's top-left corner, y down.
    let corner = match context.choice("origin") {
        0 => [0.5 - half, 0.0],
        1 => [1.0 - rad, 0.5 - half],
        2 => [0.5 - half, 1.0 - rad],
        3 => [0.0, 0.5 - half],
        4 => [1.0 - rad, 0.0],
        5 => [1.0 - rad, 1.0 - rad],
        6 => [0.0, 1.0 - rad],
        7 => [0.0, 0.0],
        _ => [0.5 - half, 0.5 - half],
    };
    let resolution = context.resolution();
    let width = [1.0 / (resolution[0] * rad), 1.0 / (resolution[1] * rad)];
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let down = [uv[0], 1.0 - uv[1]];
        let local = [(down[0] - corner[0]) / rad, (down[1] - corner[1]) / rad];
        let inside = builtin_interval_coverage(local[0], width[0])
            * builtin_interval_coverage(local[1], width[1]);
        let uv2 = [local[0], 1.0 - local[1]];
        let (moving, still) = if slide_in {
            (gl_to_color(context, uv2), gl_from_color(context, uv))
        } else {
            (gl_from_color(context, uv2), gl_to_color(context, uv))
        };
        mix4(still, moving, inside)
    });
}

/// `fold.wgsl`.
fn fold(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[0] - progress);
        let uv = gl_uv(uv);
        let unfolded = builtin_aa_step(progress, uv[0], width);
        let a = gl_from_color(context, [(uv[0] - progress) / (1.0 - progress), uv[1]]);
        let b = gl_to_color(context, [uv[0] / progress, uv[1]]);
        mix4(b, a, unfolded)
    });
}

/// `directional-easing.wgsl`.
fn directional_easing(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let heading = push_heading(context);
    let easing = ((2.0 - progress) * progress).sqrt();
    let resolution = context.resolution();
    let width = [1.0 / resolution[0], 1.0 / resolution[1]];
    let max_side = context.max_filter_samples().clamp(1.0, 16.0).sqrt().floor();
    let shift = scale(heading, easing);
    let tap = |q: Vec2, _: f32| {
        let f = [fract(q[0]), fract(q[1])];
        let inside = step(0.0, q[1]) * step(q[1], 1.0) * step(0.0, q[0]) * step(q[0], 1.0);
        mix4(gl_to_color(context, f), gl_from_color(context, f), inside)
    };
    transition_row(context, y, x0, out, |_, uv| {
        push_pixel(width, max_side, gl_uv(uv), (heading, shift), tap)
    });
}

/// `hsv-fade.wgsl`.
/// `hsv_fade_hsv`: the HSV of a picture's straight color, its saturation
/// faded out below a tenth of white.
fn hsv_fade_hsv(color: Rgba) -> [f32; 3] {
    let hsv = builtin_rgb_to_hsv(builtin_straight(color));
    [
        hsv[0],
        hsv[1] * builtin_smoothstep(0.0, 0.1, hsv[2]),
        hsv[2],
    ]
}

fn hsv_fade(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let outgoing = gl_from_color(context, uv);
        let incoming = gl_to_color(context, uv);
        let a = hsv_fade_hsv(outgoing);
        let b = hsv_fade_hsv(incoming);
        let chroma_a = [a[1] * cosf(a[0] * TAU), a[1] * sinf(a[0] * TAU)];
        let chroma_b = [b[1] * cosf(b[0] * TAU), b[1] * sinf(b[0] * TAU)];
        let chroma = [
            mix(chroma_a[0], chroma_b[0], progress),
            mix(chroma_a[1], chroma_b[1], progress),
        ];
        let hue = if dot(chroma, chroma) > 0.0 {
            atan2f(chroma[1], chroma[0]) / TAU
        } else {
            0.0
        };
        let m = [hue, length(chroma), mix(a[2], b[2], progress)];
        let alpha = mix(outgoing[3], incoming[3], progress);
        let rgb = builtin_hsv_to_rgb(m);
        [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
    });
}

/// `left-right.wgsl` and `top-bottom.wgsl`: the incoming picture at
/// `uv - offset` over the outgoing one receding towards `back_color`.
fn receding_slide(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [Rgba],
    offset: impl Fn(f32) -> Vec2,
) {
    let progress = context.progress();
    let back = builtin_premultiply(context.vec4("back_color"));
    let size = mix(1.0, 3.0, progress * 0.2);
    let shift = offset(progress);
    let resolution = context.resolution();
    let width = [1.0 / resolution[0], 1.0 / resolution[1]];
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let spto = [(uv[0] + -0.5) * size + 0.5, (uv[1] + -0.5) * size + 0.5];
        let spfr = [uv[0] - shift[0], uv[1] - shift[1]];
        let arriving = builtin_interval_coverage(spfr[0], width[0])
            * builtin_interval_coverage(spfr[1], width[1]);
        let receding = builtin_interval_coverage(spto[0], width[0] * size)
            * builtin_interval_coverage(spto[1], width[1] * size);
        let behind = mix4(
            back,
            gl_from_color(context, spto),
            receding * (1.0 - progress),
        );
        mix4(behind, gl_to_color(context, spfr), arriving)
    });
}

/// `left-right.wgsl`.
fn left_right(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    receding_slide(context, y, x0, out, |progress| [1.0 - progress, 0.0]);
}

/// `top-bottom.wgsl`: `uv + (0, 1 - progress)`, the same as subtracting its
/// negation.
fn top_bottom(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    receding_slide(context, y, x0, out, |progress| [-0.0, -(1.0 - progress)]);
}

/// `multiply-blend.wgsl`.
fn multiply_blend(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let outgoing = gl_from_color(context, uv);
        let incoming = gl_to_color(context, uv);
        let blended = [
            outgoing[0] * incoming[0],
            outgoing[1] * incoming[1],
            outgoing[2] * incoming[2],
            outgoing[3] * incoming[3],
        ];
        if progress < 0.5 {
            mix4(outgoing, blended, 2.0 * progress)
        } else {
            mix4(blended, incoming, 2.0 * progress - 1.0)
        }
    });
}

/// `rolls.wgsl`.
fn rolls(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let ratio = context.ratio();
    let corner = context.choice("corner");
    let down = context.boolean("rot_down");
    let turn = select(select(-PI, PI, down), select(PI, -PI, down), corner >= 2);
    let theta = turn / 2.0 * progress;
    let flip_x = corner == 0 || corner == 3;
    let flip_y = corner == 2 || corner == 3;
    let c1 = cosf(theta);
    let s1 = sinf(theta);
    let resolution = context.resolution();
    let width = [
        (c1.abs() + s1.abs()) / resolution[0],
        (c1.abs() + s1.abs()) / resolution[1],
    ];
    let flip = |v: Vec2| {
        [
            select(v[0], 1.0 - v[0], flip_x),
            select(v[1], 1.0 - v[1], flip_y),
        ]
    };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let uvi = flip(uv);
        let uv2 = [
            (uvi[0] * ratio * c1 - uvi[1] * s1) / ratio,
            uvi[0] * ratio * s1 + uvi[1] * c1,
        ];
        let inside = builtin_interval_coverage(uv2[0], width[0])
            * builtin_interval_coverage(uv2[1], width[1]);
        mix4(
            gl_to_color(context, uv),
            gl_from_color(context, flip(uv2)),
            inside,
        )
    });
}

/// How a split slide moves its two halves.
#[derive(Clone, Copy)]
enum Split {
    /// The incoming halves slide in over the outgoing picture.
    In,
    /// The outgoing halves slide out off the incoming picture.
    Out,
    /// The outgoing halves push the incoming ones in.
    InOut,
}

/// The six split slides: the halves either side of the middle move in
/// opposite directions along x, or along y when `vertical`.
fn split_slide(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [Rgba],
    split: Split,
    vertical: bool,
) {
    let progress = context.progress();
    let reversed = select(1.0, -1.0, context.boolean("reversed"));
    let along = usize::from(vertical);
    let across = 1 - along;
    let width = 1.0 / context.resolution()[along];
    let moved = |uv: Vec2, position: f32| {
        let mut moved = uv;
        moved[along] = position;
        moved
    };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let upper = uv[across] > 0.5;
        let position = uv[along];
        match split {
            Split::In => {
                let modifier = reversed;
                let to_position = if upper {
                    (position - (progress * modifier)) + modifier
                } else {
                    (position + (progress * modifier)) - modifier
                };
                let arrived = builtin_interval_coverage(to_position, width);
                mix4(
                    gl_from_color(context, uv),
                    gl_to_color(context, moved(uv, to_position)),
                    arrived,
                )
            }
            Split::Out | Split::InOut => {
                let modifier = select(-1.0, 1.0, upper) * reversed;
                let from_position = position + (progress * modifier);
                let staying = builtin_interval_coverage(from_position, width);
                let to_uv = match split {
                    Split::InOut => moved(uv, (position + (progress * modifier)) - modifier),
                    _ => uv,
                };
                mix4(
                    gl_to_color(context, to_uv),
                    gl_from_color(context, moved(uv, from_position)),
                    staying,
                )
            }
        }
    });
}

fn split_slide_in_horizontal(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    split_slide(context, y, x0, out, Split::In, false);
}

fn split_slide_in_vertical(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    split_slide(context, y, x0, out, Split::In, true);
}

fn split_slide_out_horizontal(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    split_slide(context, y, x0, out, Split::Out, false);
}

fn split_slide_out_vertical(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    split_slide(context, y, x0, out, Split::Out, true);
}

fn split_slide_in_out_horizontal(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    split_slide(context, y, x0, out, Split::InOut, false);
}

fn split_slide_in_out_vertical(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    split_slide(context, y, x0, out, Split::InOut, true);
}

/// `rotate.wgsl`.
fn rotate(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let angle = progress * PI * 2.0;
    let cosine = cosf(angle);
    let sine = sinf(angle);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let v = [uv[0] - 0.5, uv[1] - 0.5];
        let p = [
            fract((v[0] * cosine - v[1] * sine) + 0.5),
            fract((v[0] * sine + v[1] * cosine) + 0.5),
        ];
        mix4(gl_from_color(context, p), gl_to_color(context, p), progress)
    });
}

/// `x-axis-translation.wgsl`.
fn x_axis_translation(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[0] - progress);
        let uv = gl_uv(uv);
        let shifted = builtin_aa_step(progress, uv[0], width);
        let new_uv = [uv[0] - progress, uv[1]];
        let faded = mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            progress,
        );
        mix4(faded, gl_from_color(context, new_uv), shifted)
    });
}

/// `original_direction` of `cover.wgsl` and `uncover.wgsl`: the way the
/// picture moves, top-left uv.
fn original_direction(index: u32) -> Vec2 {
    match index {
        0 => [-1.0, 0.0],
        1 => [1.0, 0.0],
        2 => [0.0, -1.0],
        _ => [0.0, 1.0],
    }
}

/// `cover.wgsl`.
fn cover(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let d = original_direction(context.choice("direction"));
    let resolution = context.resolution();
    let width = [1.0 / resolution[0], 1.0 / resolution[1]];
    transition_row(context, y, x0, out, |_, uv| {
        let q = add(uv, scale(d, 1.0 - progress));
        let inside =
            builtin_interval_coverage(q[0], width[0]) * builtin_interval_coverage(q[1], width[1]);
        let top = context.to_color(q).map(|channel| channel * inside);
        builtin_over(top, context.from_color(uv))
    });
}

/// `uncover.wgsl`.
fn uncover(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let d = original_direction(context.choice("direction"));
    let resolution = context.resolution();
    let width = [1.0 / resolution[0], 1.0 / resolution[1]];
    transition_row(context, y, x0, out, |_, uv| {
        let q = sub(uv, scale(d, progress));
        let inside =
            builtin_interval_coverage(q[0], width[0]) * builtin_interval_coverage(q[1], width[1]);
        let top = context.from_color(q).map(|channel| channel * inside);
        builtin_over(top, context.to_color(uv))
    });
}
