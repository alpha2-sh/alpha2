//! The "Wipes" group's kernels.

use super::shapes::gl_aa_step;
use super::{
    KernelContext, PI, Rgba, Transition, Vec2, atan2f, builtin_aa_pulse, builtin_aa_smoothstep,
    builtin_aa_step, builtin_angle_width, builtin_smoothstep, cosf, dot, fract, fwidth_fine,
    gl_from_color, gl_to_color, gl_uv, l1_direction, length, mix, mix4, pcg, select, sinf,
    smoothstep, step, sub, transition_row,
};
use crate::cpu::kernels::{
    builtin_cell, builtin_hash, builtin_hash3, builtin_unit, builtin_value_noise,
};

pub(super) static KERNELS: &[(&str, Transition)] = &[
    ("wipe-up", Transition(wipe_up)),
    ("wipe-down", Transition(wipe_down)),
    ("wipe-left", Transition(wipe_left)),
    ("wipe-right", Transition(wipe_right)),
    ("window-blinds", Transition(window_blinds)),
    ("window-slice", Transition(window_slice)),
    ("wind", Transition(wind)),
    ("pinwheel", Transition(pinwheel)),
    ("heart", Transition(heart)),
    ("horizontal-open", Transition(horizontal_open)),
    ("horizontal-close", Transition(horizontal_close)),
    ("vertical-open", Transition(vertical_open)),
    ("vertical-close", Transition(vertical_close)),
    ("screen-melt", Transition(screen_melt)),
    ("squares-wire", Transition(squares_wire)),
    ("static-wipe", Transition(static_wipe)),
    ("zoom-left-wipe", Transition(zoom_left_wipe)),
    ("zoom-right-wipe", Transition(zoom_right_wipe)),
    ("gradient-wipe", Transition(gradient_wipe)),
];

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

/// The four barn doors: `axis` 1 (y) for the horizontal ones, 0 for the
/// vertical; opening or closing.
fn barn_door(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [Rgba],
    axis: usize,
    opening: bool,
) {
    let progress = context.progress();
    let regress = 1.0 - progress;
    let s = |uv: Vec2| {
        if opening {
            2.0 - ((uv[axis] - 0.5) / (regress - 1.0)).abs() - 2.0 * regress
        } else {
            2.0 - ((uv[axis] - 0.5) / (progress - 1.0)).abs() - 2.0 * progress
        }
    };
    let (edge0, edge1) = if opening { (0.0, 0.5) } else { (0.5, 0.0) };
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| s(gl_uv(uv)));
        let uv = gl_uv(uv);
        let amount = builtin_aa_smoothstep(edge0, edge1, s(uv), width);
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), amount)
    });
}

fn horizontal_open(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    barn_door(context, y, x0, out, 1, true);
}

fn horizontal_close(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    barn_door(context, y, x0, out, 1, false);
}

fn vertical_open(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    barn_door(context, y, x0, out, 0, true);
}

fn vertical_close(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    barn_door(context, y, x0, out, 0, false);
}

fn screen_melt(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let bars = context.float("bars").round_ties_even().max(2.0);
    let amplitude = context.float("amplitude");
    let noise = context.float("noise");
    let frequency = context.float("frequency");
    let drip_scale = context.float("drip_scale");
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[1]).max(0.000_000_1);
        let uv = gl_uv(uv);
        let bar = (uv[0] * bars).floor();
        let wave = {
            let x = bar * frequency * 0.1 * bars;
            cosf(x * 0.5) * cosf(x * 0.13) * sinf((x + 10.0) * 0.3) / 2.0 + 0.5
        };
        let jitter = builtin_unit(builtin_hash(bar.max(0.0) as u32));
        // The original's own constant, not pi.
        #[allow(clippy::approx_constant)]
        let drip = sinf(bar / (bars - 1.0) * 3.141_592) * drip_scale;
        let scale = 1.0 + (mix(wave, jitter, noise) + drip) * amplitude;
        let phase = progress * scale;
        let landed = (0.5 + (phase + uv[1] - 1.0) / width).clamp(0.0, 1.0);
        mix4(
            gl_from_color(context, [uv[0], uv[1] + phase]),
            gl_to_color(context, uv),
            landed,
        )
    });
}

#[allow(clippy::many_single_char_names)]
fn squares_wire(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let squares = context.vec2("squares").map(f32::round_ties_even);
    let v = l1_direction(context.vec2("direction"), [1.0, -0.5]);
    let offset = context.float("smoothness");
    let d = v[0] * 0.5 + v[1] * 0.5;
    let front = |p: Vec2| v[0] * p[0] + v[1] * p[1] - (d - 0.5 + progress * (1.0 + offset));
    transition_row(context, y, x0, out, |x, uv| {
        let width = [
            fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[0] * squares[0]),
            fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[1] * squares[1]),
        ];
        let front_width = fwidth_fine(context.size, x, y, |uv| front(gl_uv(uv)));
        let p = gl_uv(uv);
        let grid = [p[0] * squares[0], p[1] * squares[1]];
        let pr = builtin_aa_smoothstep(-offset, 0.0, front(p), front_width);
        let gap = pr / 2.0;
        let a = builtin_aa_pulse(grid[0] - gap, 1.0 - pr, width[0])
            * builtin_aa_pulse(grid[1] - gap, 1.0 - pr, width[1]);
        mix4(gl_from_color(context, p), gl_to_color(context, p), a)
    });
}

fn static_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let up_to_down = context.boolean("up_to_down");
    let span = context.float("max_static_span") * sinf(PI * progress).max(0.0).sqrt();
    let resolution = context.resolution();
    let salt = progress.to_bits();
    let edge = |uv: Vec2| select(uv[1], 1.0 - uv[1], up_to_down);
    let size = context.size;
    transition_row(context, y, x0, out, |x, uv| {
        let mix_ratio = 1.0 - gl_aa_step(size, x, y, |uv| (progress, edge(uv)));
        let uv = gl_uv(uv);
        let wiped = mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            mix_ratio,
        );
        let envelope = builtin_smoothstep(progress - span, progress, edge(uv))
            * (1.0 - builtin_smoothstep(progress, progress + span, edge(uv)));
        let value = builtin_unit(builtin_hash3(
            builtin_cell(uv[0] * resolution[0]),
            builtin_cell(uv[1] * resolution[1]),
            salt,
        ));
        mix4(wiped, [value, value, value, 1.0], envelope)
    });
}

/// `zoom-left-wipe` and `zoom-right-wipe`: a zoom punch, then a wipe whose edge is
/// `edge(uv)`.
fn zoom_wipe(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [Rgba],
    edge: impl Fn(Vec2) -> f32,
) {
    let progress = context.progress();
    let t = (progress - 0.5) * 2.0;
    let quick = context.float("zoom_quickness").clamp(0.0, 0.5);
    let amount = builtin_smoothstep(0.0, quick, progress);
    let zoom = select(amount, 1.0 - amount, amount < 0.5);
    let size = context.size;
    transition_row(context, y, x0, out, |x, uv| {
        let wiped = gl_aa_step(size, x, y, |uv| (edge(uv), t));
        let uv = gl_uv(uv);
        if progress < 0.5 {
            gl_from_color(
                context,
                [0.5 + (uv[0] - 0.5) * zoom, 0.5 + (uv[1] - 0.5) * zoom],
            )
        } else {
            mix4(gl_from_color(context, uv), gl_to_color(context, uv), wiped)
        }
    });
}

fn zoom_left_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    zoom_wipe(context, y, x0, out, |uv| 1.0 - uv[0]);
}

fn zoom_right_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    zoom_wipe(context, y, x0, out, |uv| uv[0]);
}

/// `wipe_value`: the gradient's value at `p`, height units about the
/// center.
fn gradient_value(p: Vec2, pattern: u32, angle: f32, noise_scale: f32, seed: u32) -> f32 {
    match pattern {
        0 => {
            let radians = angle * PI / 180.0;
            dot(p, [cosf(radians), -sinf(radians)])
        }
        2 => fract(atan2f(-p[1], p[0]) / core::f32::consts::TAU - angle / 360.0),
        3 => p[0].abs() + p[1].abs(),
        4 => {
            let q = [p[0] / noise_scale, p[1] / noise_scale];
            builtin_value_noise(q, seed) * 0.6667
                + builtin_value_noise([q[0] * 2.0 + 17.0, q[1] * 2.0 + 5.0], seed) * 0.3333
        }
        _ => length(p),
    }
}

fn gradient_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let pattern = context.choice("pattern");
    let angle = context.float("angle");
    let center = context.vec2("center");
    let softness = context.float("softness");
    let noise_scale = context.float("noise_scale");
    let seed = context
        .float("seed")
        .round_ties_even()
        .clamp(0.0, 16_777_215.0) as u32;
    let reversed = context.boolean("reversed");
    let resolution = context.resolution();
    let value_at = |uv: Vec2| {
        let p = [
            (uv[0] - center[0]) * (resolution[0] / resolution[1]),
            (uv[1] - center[1]) * 1.0,
        ];
        gradient_value(p, pattern, angle, noise_scale, seed)
    };
    let c = [
        value_at([0.0, 0.0]),
        value_at([1.0, 0.0]),
        value_at([0.0, 1.0]),
        value_at([1.0, 1.0]),
    ];
    let from_center = pattern == 1 || pattern == 3;
    let unit_span = pattern == 2 || pattern == 4;
    let low = select(
        select(c[0].min(c[1]).min(c[2].min(c[3])), 0.0, from_center),
        0.0,
        unit_span,
    );
    let high = select(c[0].max(c[1]).max(c[2].max(c[3])), 1.0, unit_span);
    let gradient = |uv: Vec2| {
        let g = ((value_at(uv) - low) / (high - low).max(0.000_001)).clamp(0.0, 1.0);
        select(g, 1.0 - g, reversed)
    };
    let t = progress * (1.0 + softness);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| t - gradient(uv));
        let value = gradient(uv);
        let hard = builtin_aa_step(value, t, width);
        let soft = builtin_smoothstep(value, value + softness, t);
        mix4(
            context.from_color(uv),
            context.to_color(uv),
            select(soft, hard, softness == 0.0),
        )
    });
}
