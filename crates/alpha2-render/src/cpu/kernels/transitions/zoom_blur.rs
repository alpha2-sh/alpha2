//! The "Zoom & Blur" group's kernels.

// The shaders' 3.14159265 and 6.28318531 are these as `f32`s.
use core::f32::consts::{PI, TAU};

use libm::{atan2f, cosf, powf, sinf};

use super::{
    KernelContext, Rgba, Transition, Vec2, add, add4, builtin_interval_coverage,
    builtin_premultiply, builtin_smoothstep, dot, fract, fwidth_fine, gl_from_color, gl_to_color,
    gl_uv, length, mix, mix4, scale, step, sub, transition_row,
};
use crate::cpu::kernels::{
    builtin_aa_step, builtin_cell, builtin_hash3, builtin_unit, builtin_valid,
};

pub(super) static KERNELS: &[(&str, Transition)] = &[
    ("defocus-blur", Transition(defocus_blur)),
    ("dreamy-zoom", Transition(dreamy_zoom)),
    ("revolve", Transition(revolve)),
    ("linear-blur", Transition(linear_blur)),
    ("mosaic-tiles", Transition(mosaic_tiles)),
    ("rotate-scale-fade", Transition(rotate_scale_fade)),
    ("rotate-scale-vanish", Transition(rotate_scale_vanish)),
    ("scale-in", Transition(scale_in)),
    ("simple-zoom-out", Transition(simple_zoom_out)),
    ("zoom-in-circles", Transition(zoom_in_circles)),
    ("zoom-in-out", Transition(zoom_in_out)),
    ("zoom-blur", Transition(zoom_blur)),
    ("whip-pan", Transition(whip_pan)),
];

/// A tap loop's count: `u32(clamp(max_filter_samples(), 1.0, 64.0))`.
fn taps(context: &KernelContext<'_>) -> u32 {
    context.max_filter_samples().clamp(1.0, 64.0) as u32
}

#[inline]
fn divide4(color: Rgba, divisor: f32) -> Rgba {
    [
        color[0] / divisor,
        color[1] / divisor,
        color[2] / divisor,
        color[3] / divisor,
    ]
}

#[inline]
fn scale4(color: Rgba, factor: f32) -> Rgba {
    [
        color[0] * factor,
        color[1] * factor,
        color[2] * factor,
        color[3] * factor,
    ]
}

/// `fwidthFine` of both components of a uv-valued field.
fn uv_width(context: &KernelContext<'_>, x: u32, y: u32, field: impl Fn(Vec2) -> Vec2) -> Vec2 {
    [
        fwidth_fine(context.size, x, y, |uv| field(uv)[0]),
        fwidth_fine(context.size, x, y, |uv| field(uv)[1]),
    ]
}

/// `defocus-blur.wgsl`.
fn defocus_blur(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let taps = taps(context);
    let progress = context.progress();
    let blur_size = context.float("blur_size");
    let radius = if progress < 0.5 {
        mix(0.0, blur_size, progress / 0.5)
    } else {
        mix(blur_size, 0.0, (progress - 0.5) / 0.5)
    };
    let reach = [radius / context.ratio(), radius];
    let offsets: Vec<Vec2> = (0..taps)
        .map(|i| {
            let ring = ((i as f32 + 0.5) / taps as f32).sqrt();
            let angle = fract(i as f32 * 0.381_966_02) * TAU;
            [cosf(angle) * ring * reach[0], sinf(angle) * ring * reach[1]]
        })
        .collect();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut c0 = [0.0; 4];
        let mut c1 = [0.0; 4];
        for offset in &offsets {
            let tap = add(uv, *offset);
            c0 = add4(c0, gl_from_color(context, tap));
            c1 = add4(c1, gl_to_color(context, tap));
        }
        mix4(divide4(c0, taps as f32), divide4(c1, taps as f32), progress)
    });
}

/// `dreamy-zoom.wgsl`.
fn dreamy_zoom(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let ratio = context.ratio();
    let zoom_scale = context.float("scale");
    let first = progress < 0.5;
    let phase = if first {
        progress * 2.0
    } else {
        (progress - 0.5) * 2.0
    };
    let turn = context.float("rotation").to_radians();
    let (angle_offset, zoom, flash) = if first {
        (
            mix(0.0, turn, phase),
            mix(1.0, zoom_scale, phase),
            mix(0.0, 1.0, phase),
        )
    } else {
        (
            mix(-turn, 0.0, phase),
            mix(zoom_scale, 1.0, phase),
            mix(1.0, 0.0, phase),
        )
    };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let p = [(uv[0] - 0.5) / zoom * ratio, (uv[1] - 0.5) / zoom * 1.0];
        let angle = atan2f(p[1], p[0]) + angle_offset;
        let dist = length(p);
        let q = [cosf(angle) * dist / ratio + 0.5, sinf(angle) * dist + 0.5];
        let c = if first {
            gl_from_color(context, q)
        } else {
            gl_to_color(context, q)
        };
        builtin_valid([c[0] + flash, c[1] + flash, c[2] + flash, c[3] + flash])
    });
}

fn revolve_envelope(t: f32) -> f32 {
    let a = ((t - 0.10) / 0.33).clamp(0.0, 1.0);
    let b = ((t - 0.43) / 0.29).clamp(0.0, 1.0);
    a * a * (3.0 - 2.0 * a) * (1.0 - b * b * (3.0 - 2.0 * b))
}

/// One motion-blur sub-step of `revolve`: what the shader's loop body
/// computes from the step alone.
struct RevolveStep {
    weight: f32,
    edge: f32,
    core: f32,
    zoom: f32,
    barrel: f32,
    reveal: f32,
}

/// `revolve.wgsl`.
#[allow(clippy::float_cmp)]
fn revolve(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let taps = taps(context);
    let progress = context.progress();
    let ratio = context.ratio();
    let center = context.vec2("center");
    let direction = context.float("direction");
    let peak_zoom = context.float("peak_zoom");
    let barrel = context.float("barrel");
    let switch_start = context.float("switch_start");
    let shadow = context.float("shadow");
    let e0 = revolve_envelope(progress);
    let span = 0.060 * context.float("motion_blur") * e0;
    let edge_turn = context.float("max_rotation").to_radians();
    let core_turn = context.float("swirl").to_radians();
    let switch_span = context.float("switch_end") - switch_start;
    let hard = switch_span == 0.0;
    let inverse_span = if hard { 0.0 } else { 1.0 / switch_span };
    let steps: Vec<RevolveStep> = (0..taps)
        .map(|i| {
            let offset = (i as f32 + 0.5) / taps as f32 * 2.0 - 1.0;
            let w0 = 1.0 - offset.abs();
            let at = (progress + offset * span).clamp(0.0, 1.0);
            let rise = ((at - 0.10) / 0.33).clamp(0.0, 1.0);
            let fall = ((at - 0.43) / 0.29).clamp(0.0, 1.0);
            let envelope =
                rise * rise * (3.0 - 2.0 * rise) * (1.0 - fall * fall * (3.0 - 2.0 * fall));
            let ramp = ((at - switch_start) * inverse_span).clamp(0.0, 1.0);
            RevolveStep {
                weight: w0 * w0 + 0.01,
                edge: edge_turn * envelope,
                core: core_turn * envelope,
                zoom: 1.0 + (peak_zoom - 1.0) * powf(envelope, 0.85),
                barrel: barrel * envelope,
                reveal: if hard {
                    step(switch_start, at)
                } else {
                    ramp * ramp * (3.0 - 2.0 * ramp)
                },
            }
        })
        .collect();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        // The same for every step: the shader recomputes it in its loop.
        let offset = [(uv[0] - center[0]) * ratio, (uv[1] - center[1]) * 1.0];
        let core = powf(1.0 - (length(offset) / 0.96).clamp(0.0, 1.0), 1.55);
        let mut color = [0.0; 4];
        let mut total = 0.0;
        for tap in &steps {
            let visible = direction * (tap.edge + tap.core * core);
            let cos_visible = cosf(-visible);
            let sin_visible = sinf(-visible);
            let turned = [
                cos_visible * offset[0] - sin_visible * offset[1],
                sin_visible * offset[0] + cos_visible * offset[1],
            ];
            let zoomed = [turned[0] / tap.zoom, turned[1] / tap.zoom];
            let rr = length(zoomed);
            let bent = scale(zoomed, 1.0 + tap.barrel * rr * rr * 2.8);
            let warped = [
                (bent[0] / ratio + center[0]).clamp(0.001, 0.999),
                (bent[1] + center[1]).clamp(0.001, 0.999),
            ];
            let flipped = gl_uv(warped);
            let blend = mix4(
                context.from_color(flipped),
                context.to_color(flipped),
                tap.reveal,
            );
            color = add4(color, scale4(blend, tap.weight));
            total += tap.weight;
        }
        let color = divide4(color, total);
        let centred = [(uv[0] - 0.5) * ratio, (uv[1] - 0.5) * 1.0];
        let vignette = 1.0 - shadow * e0 * builtin_smoothstep(0.35, 0.95, length(centred));
        [
            color[0] * vignette,
            color[1] * vignette,
            color[2] * vignette,
            color[3],
        ]
    });
}

/// `linear-blur.wgsl`.
fn linear_blur(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let side = context.max_filter_samples().clamp(1.0, 64.0).sqrt().floor();
    let disp = context.float("intensity") * (0.5 - (0.5 - progress).abs());
    let reach = [disp / context.ratio(), disp];
    let mut offsets = Vec::new();
    for xi in 0..side as u32 {
        for yi in 0..side as u32 {
            let v = [
                (xi as f32 + 0.5) / side - 0.5,
                (yi as f32 + 0.5) / side - 0.5,
            ];
            offsets.push([v[0] * reach[0], v[1] * reach[1]]);
        }
    }
    let count = side * side;
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut c1 = [0.0; 4];
        let mut c2 = [0.0; 4];
        for offset in &offsets {
            let p = add(uv, *offset);
            c1 = add4(c1, gl_from_color(context, p));
            c2 = add4(c2, gl_to_color(context, p));
        }
        mix4(divide4(c1, count), divide4(c2, count), progress)
    });
}

/// `mosaic-tiles.wgsl`.
#[allow(clippy::float_cmp)]
fn mosaic_tiles(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let rpr = progress * 2.0 - 1.0;
    let az = (-(rpr * rpr * 2.0) + 3.0).abs();
    let goal = [
        (context.float("end_x") + 0.5).floor(),
        (context.float("end_y") + 0.5).floor(),
    ];
    let ease = -cosf(progress * PI) / 2.0 + 0.5;
    let travel = [
        mix(0.5, goal[0] + 0.5, ease * ease),
        mix(0.5, goal[1] + 0.5, ease * ease),
    ];
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let rp = [
            (uv[0] - 0.5) * az + travel[0],
            (uv[1] - 0.5) * az + travel[1],
        ];
        let tile = [rp[0].floor(), rp[1].floor()];
        let within = sub(rp, tile);
        let on_end = tile == goal;
        let value = builtin_unit(builtin_hash3(builtin_cell(rp[0]), builtin_cell(rp[1]), 0));
        let quarter = if on_end { 0 } else { (value * 4.0) as u32 };
        let centred = [within[0] - 0.5, within[1] - 0.5];
        let turned = match quarter {
            1 => [centred[1], -centred[0]],
            2 => [-centred[0], -centred[1]],
            3 => [-centred[1], centred[0]],
            _ => centred,
        };
        let tile_uv = add([0.5, 0.5], turned);
        if on_end || value > 0.5 {
            gl_to_color(context, tile_uv)
        } else {
            gl_from_color(context, tile_uv)
        }
    });
}

/// `rotate-scale-fade.wgsl`.
fn rotate_scale_fade(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let ratio = context.ratio();
    let center = context.vec2("center");
    let back_color = builtin_premultiply(context.vec4("back_color"));
    let angle = TAU * fract(context.float("rotations") * progress);
    let c = cosf(angle);
    let s = sinf(angle);
    let current_scale = mix(context.float("scale"), 1.0, 2.0 * (progress - 0.5).abs());
    let turned_uv = |uv: Vec2| {
        let uv = gl_uv(uv);
        let difference = [(uv[0] - center[0]) * ratio, (uv[1] - center[1]) * 1.0];
        let turned = [
            difference[0] * c - difference[1] * s,
            difference[0] * s + difference[1] * c,
        ];
        [
            center[0] + turned[0] / current_scale / ratio,
            center[1] + turned[1] / current_scale / 1.0,
        ]
    };
    transition_row(context, y, x0, out, |x, uv| {
        let width = uv_width(context, x, y, turned_uv);
        let q = turned_uv(uv);
        let inside =
            builtin_interval_coverage(q[0], width[0]) * builtin_interval_coverage(q[1], width[1]);
        let image = mix4(gl_from_color(context, q), gl_to_color(context, q), progress);
        mix4(back_color, image, inside)
    });
}

/// `rotate-scale-vanish.wgsl`.
fn rotate_scale_vanish(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let ratio = context.ratio();
    let reversed = context.boolean("reverse_effect");
    let back_color = builtin_premultiply(context.vec4("back_color"));
    let fade_in_second = context.boolean("fade_in_second");
    let t = if reversed { 1.0 - progress } else { progress };
    let theta = if context.boolean("reverse_rotation") {
        TAU * t
    } else {
        -TAU * t
    };
    let c1 = cosf(theta);
    let s1 = sinf(theta);
    let rad = (1.0 - t).max(0.00001);
    let shrunk_uv = |uv: Vec2| {
        let uv = gl_uv(uv);
        let centred = [(uv[0] - 0.5) * ratio, (uv[1] - 0.5) * 1.0];
        let turned = [
            (centred[0] * c1 - centred[1] * s1) / rad,
            (centred[0] * s1 + centred[1] * c1) / rad,
        ];
        [
            (turned[0] + ratio / 2.0) / ratio,
            (turned[1] + 1.0 / 2.0) / 1.0,
        ]
    };
    transition_row(context, y, x0, out, |x, uv| {
        let width = uv_width(context, x, y, shrunk_uv);
        let q = shrunk_uv(uv);
        let uv = gl_uv(uv);
        let inside =
            builtin_interval_coverage(q[0], width[0]) * builtin_interval_coverage(q[1], width[1]);
        let (color_to, shrinking) = if reversed {
            (gl_from_color(context, uv), gl_to_color(context, q))
        } else {
            (gl_to_color(context, uv), gl_from_color(context, q))
        };
        let outside = if fade_in_second { back_color } else { color_to };
        let col3 = mix4(outside, shrinking, inside);
        add4(scale4(col3, 1.0 - t), scale4(color_to, t))
    });
}

/// `scale-in.wgsl`.
fn scale_in(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let scaled = add([0.5, 0.5], scale(sub(uv, [0.5, 0.5]), progress));
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, scaled),
            progress,
        )
    });
}

/// `simple-zoom-out.wgsl`.
fn simple_zoom_out(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let quick = context.float("zoom_quickness").clamp(0.2, 1.0);
    let amount = builtin_smoothstep(1.0 - quick, 1.0, progress);
    let cut = if progress < 1.0 - quick { 0.0 } else { 1.0 };
    let weight = if context.boolean("fade") { amount } else { cut };
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let zoomed = add([0.5, 0.5], scale(sub(uv, [0.5, 0.5]), 1.0 - (1.0 - amount)));
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, zoomed),
            weight,
        )
    });
}

/// `zoom-in-circles.wgsl`'s `zoom_ring`.
fn zoom_ring(context: &KernelContext<'_>, uv: Vec2, amount: f32, mixed: f32) -> Rgba {
    let p = add([0.5, 0.5], scale(sub(uv, [0.5, 0.5]), amount));
    mix4(gl_from_color(context, p), gl_to_color(context, p), mixed)
}

/// `zoom-in-circles.wgsl`.
fn zoom_in_circles(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let ratio2 = [1.0, 1.0 / context.ratio()];
    let pro = progress / 0.8;
    let late = pro > 1.0;
    let z = if late {
        0.2 + (pro - 1.0) * 5.0
    } else {
        pro * 0.2
    };
    let t = if late {
        ((progress - 0.8) / 0.07).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let edges = [0.5 + z, 0.8 + z * 1.5, 1.2 + z * 2.5];
    let ring_length = |uv: Vec2| {
        let uv = gl_uv(uv);
        length([
            2.0 * ((uv[0] - 0.5) * ratio2[0]),
            2.0 * ((uv[1] - 0.5) * ratio2[1]),
        ])
    };
    transition_row(context, y, x0, out, |x, uv| {
        let l = ring_length(uv);
        let beyond = edges.map(|edge| {
            let width = fwidth_fine(context.size, x, y, |uv| ring_length(uv) - edge);
            builtin_aa_step(edge, l, width)
        });
        let uv = gl_uv(uv);
        let ring0 = zoom_ring(context, uv, 1.0, t);
        let ring1 = zoom_ring(context, uv, 1.0 - 0.15 * pro, t * 0.5);
        let ring2 = zoom_ring(context, uv, 1.0 - 0.2 * pro, t * 0.2);
        let ring3 = zoom_ring(context, uv, 1.0 - 0.25 * pro, t);
        mix4(
            ring0,
            mix4(ring1, mix4(ring2, ring3, beyond[2]), beyond[1]),
            beyond[0],
        )
    });
}

/// `zoom-in-out.wgsl`.
fn zoom_in_out(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let zoom_from = builtin_smoothstep(0.0, 1.0, progress * 2.0);
    let zoom_to = builtin_smoothstep(0.0, 1.0, (1.0 - progress) * 2.0);
    let crossfade = builtin_smoothstep(0.4, 0.6, progress);
    let zoom_uv = |uv: Vec2, amount: f32| add([0.5, 0.5], scale(sub(uv, [0.5, 0.5]), 1.0 - amount));
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        mix4(
            gl_from_color(context, zoom_uv(uv, zoom_from)),
            gl_to_color(context, zoom_uv(uv, zoom_to)),
            crossfade,
        )
    });
}

/// `zoom-blur.wgsl`.
fn zoom_blur(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let taps = taps(context);
    let progress = context.progress();
    let center = context.vec2("center");
    let reach = context.float("strength") * sinf(PI * progress);
    let blend = builtin_smoothstep(0.35, 0.65, progress);
    let fractions: Vec<f32> = (0..taps)
        .map(|i| (i as f32 + 0.5) / taps as f32 * reach)
        .collect();
    transition_row(context, y, x0, out, |_, uv| {
        let toward = sub(center, uv);
        let mut sum = [0.0; 4];
        for fraction in &fractions {
            let p = add(uv, scale(toward, *fraction));
            sum = add4(sum, mix4(context.from_color(p), context.to_color(p), blend));
        }
        divide4(sum, taps as f32)
    });
}

/// `whip-pan.wgsl`.
fn whip_pan(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let taps = taps(context);
    let progress = context.progress();
    let d: Vec2 = match context.choice("direction") {
        0 => [-1.0, 0.0],
        1 => [1.0, 0.0],
        2 => [0.0, -1.0],
        _ => [0.0, 1.0],
    };
    let axis = [d[0].abs(), d[1].abs()];
    let resolution = context.resolution();
    let texel = dot(axis, [1.0 / resolution[0], 1.0 / resolution[1]]);
    let streak = (context.float("blur") * 0.5 * sinf(PI * progress)).max(texel);
    let forward = dot(d, axis) > 0.0;
    let shifts: Vec<Vec2> = (0..taps)
        .map(|i| scale(d, streak * ((i as f32 + 0.5) / taps as f32 - 0.5)))
        .collect();
    transition_row(context, y, x0, out, |_, uv| {
        let start = sub(uv, scale(d, progress));
        let mut sum = [0.0; 4];
        for shift in &shifts {
            let q = add(start, *shift);
            let along = dot(q, axis);
            let incoming = if forward { along < 0.0 } else { along > 1.0 };
            let color = if incoming {
                context.to_color(add(q, d))
            } else {
                context.from_color(q)
            };
            sum = add4(sum, color);
        }
        divide4(sum, taps as f32)
    });
}
