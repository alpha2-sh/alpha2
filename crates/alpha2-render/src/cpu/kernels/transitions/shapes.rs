//! The "Shapes" group's kernels.

use super::{
    KernelContext, PI, Rgba, Transition, Vec2, atan2f, builtin_aa_ramp, builtin_aa_step,
    builtin_premultiply, cosf, dot, fract, fwidth_fine, gl_from_color, gl_to_color, gl_uv, length,
    mix, mix4, select, sinf, sub, transition_row,
};
use crate::RenderSize;
use crate::cpu::kernels::{builtin_floor_mod, pixel_uv};

pub(super) static KERNELS: &[(&str, Transition)] = &[
    ("box", Transition(box_transition)),
    ("chessboard", Transition(chessboard)),
    ("bow-tie", Transition(bow_tie)),
    ("bow-tie-horizontal", Transition(bow_tie_horizontal)),
    ("bow-tie-vertical", Transition(bow_tie_vertical)),
    ("fragment", Transition(fragment)),
    ("polar-function", Transition(polar_function)),
    ("rectangle", Transition(rectangle)),
    ("rectangle-crop", Transition(rectangle_crop)),
    ("star-wipe", Transition(star_wipe)),
    ("shape-wipe", Transition(shape_wipe)),
];

/// `builtin_aa_step(edge, value)` at pixel `(x, y)`, where `field` gives
/// both from the pixel's gl-transitions uv.
pub(super) fn gl_aa_step(
    size: RenderSize,
    x: u32,
    y: u32,
    field: impl Fn(Vec2) -> (f32, f32),
) -> f32 {
    let width = fwidth_fine(size, x, y, |uv| {
        let (edge, value) = field(gl_uv(uv));
        value - edge
    });
    let (edge, value) = field(gl_uv(pixel_uv(size, x, y)));
    builtin_aa_step(edge, value, width)
}

fn box_transition(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let rect_in = context.boolean("rect_in");
    let location = context.choice("location");
    let p = select(progress, 1.0 - progress, rect_in);
    let centered = location == 0;
    let left = location == 1 || location == 2;
    let top = location == 1 || location == 3;
    let inset = 0.5 * (1.0 - p);
    let x1 = select(select(1.0 - p, 0.0, left), inset, centered);
    let y1 = select(select(0.0, 1.0 - p, top), inset, centered);
    let x2 = select(select(1.0, p, left), 1.0 - inset, centered);
    let y2 = select(select(p, 1.0, top), 1.0 - inset, centered);
    let size = context.size;
    transition_row(context, y, x0, out, |x, uv| {
        let in_rect = gl_aa_step(size, x, y, |uv| (x1, uv[0]))
            * gl_aa_step(size, x, y, |uv| (uv[0], x2))
            * gl_aa_step(size, x, y, |uv| (y1, uv[1]))
            * gl_aa_step(size, x, y, |uv| (uv[1], y2));
        let uv = gl_uv(uv);
        mix4(
            gl_from_color(context, uv),
            gl_to_color(context, uv),
            select(in_rect, 1.0 - in_rect, rect_in),
        )
    });
}

fn chessboard(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let grid_num = context.float("grid_num");
    let first = progress <= 0.5;
    let t = select((progress - 0.5) * 2.0, progress * 2.0, first);
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[0] * grid_num).max(0.000_000_1);
        let uv = gl_uv(uv);
        let st = [uv[0] * grid_num, uv[1] * grid_num];
        let checker = builtin_floor_mod(st[0].floor() + st[1].floor(), 2.0);
        let wipe = select(
            (0.5 + (t - fract(st[0])) / width).clamp(0.0, 1.0),
            1.0,
            t >= 1.0,
        );
        let amount = select(
            select(1.0, wipe, checker < 0.5),
            select(0.0, wipe, checker > 0.5),
            first,
        );
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), amount)
    });
}

/// `bow_tie_edge`: the signed distance from `p` to the line through `a`
/// and `b`, positive on the side `o` picks.
#[allow(clippy::many_single_char_names)]
fn bow_tie_edge(p: Vec2, a: Vec2, b: Vec2, o: f32) -> f32 {
    let e = sub(b, a);
    (e[0] * (p[1] - a[1]) - e[1] * (p[0] - a[0])) * o / length(e).max(0.000_001)
}

/// `bow_tie_triangle`: positive inside the triangle `abc` of either
/// winding.
#[allow(clippy::many_single_char_names)]
fn bow_tie_triangle(p: Vec2, a: Vec2, b: Vec2, c: Vec2) -> f32 {
    let o = select(
        -1.0,
        1.0,
        (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]) >= 0.0,
    );
    bow_tie_edge(p, a, b, o)
        .min(bow_tie_edge(p, b, c, o))
        .min(bow_tie_edge(p, c, a, o))
}

/// The three bow ties: `incoming(uv)` is positive where the incoming
/// picture shows.
fn bow_tie_shapes(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [Rgba],
    incoming: impl Fn(Vec2) -> f32,
) {
    let size = context.size;
    transition_row(context, y, x0, out, |x, uv| {
        let shown = gl_aa_step(size, x, y, |uv| (0.0, incoming(uv)));
        let uv = gl_uv(uv);
        mix4(gl_from_color(context, uv), gl_to_color(context, uv), shown)
    });
}

fn bow_tie(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let adjust = context.float("adjust");
    let reversed = context.boolean("reversed");
    let prog = select(progress, 1.0 - progress, reversed);
    let first = select_bool(progress < 0.5, progress >= 0.5, reversed);
    let height = 0.5;
    let s = 2.0 * (prog - 0.5);
    bow_tie_shapes(context, y, x0, out, |uv| {
        let bottom = bow_tie_triangle(
            uv,
            [0.0, prog - height],
            [1.0, prog - height],
            [adjust, prog],
        );
        let top = bow_tie_triangle(
            uv,
            [0.0, 1.0 - prog + height],
            [1.0, 1.0 - prog + height],
            [adjust, 1.0 - prog],
        );
        let right = bow_tie_triangle(
            uv,
            [prog + height, 1.0],
            [prog + height, 0.0],
            [mix(adjust, 1.0, s), 0.5],
        );
        let left = bow_tie_triangle(
            uv,
            [1.0 - prog - height, 1.0],
            [1.0 - prog - height, 0.0],
            [mix(adjust, 0.0, s), 0.5],
        );
        select(-right.max(left), bottom.max(top), first)
    });
}

const fn select_bool(rejected: bool, accepted: bool, condition: bool) -> bool {
    if condition { accepted } else { rejected }
}

fn bow_tie_horizontal(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let p = context.progress();
    bow_tie_shapes(context, y, x0, out, |uv| {
        let left = bow_tie_triangle(uv, [p, 0.5], [0.0, 0.5 - p], [0.0, 0.5 + p]);
        let right = bow_tie_triangle(uv, [1.0 - p, 0.5], [1.0, 0.5 - p], [1.0, 0.5 + p]);
        left.max(right)
    });
}

fn bow_tie_vertical(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let p = context.progress();
    bow_tie_shapes(context, y, x0, out, |uv| {
        let top = bow_tie_triangle(uv, [0.5, p], [0.5 - p, 0.0], [0.5 + p, 0.0]);
        let bottom = bow_tie_triangle(uv, [0.5, 1.0 - p], [0.5 - p, 1.0], [0.5 + p, 1.0]);
        top.max(bottom)
    });
}

/// `fragment.wgsl`'s shard sites, PCG hashes `builtin_hash3(shard, k, 0)`.
#[allow(clippy::unreadable_literal)]
const FRAGMENT_SITES: [Vec2; 10] = [
    [0.49947667, 0.79509515],
    [0.78598905, 0.8087231],
    [0.7057787, 0.054352045],
    [0.15288764, 0.93668187],
    [0.144001, 0.630305],
    [0.7376622, 0.4944077],
    [0.96819806, 0.06137538],
    [0.052491367, 0.2105034],
    [0.84342873, 0.49684906],
    [0.5713028, 0.6560287],
];

/// `fragment.wgsl`'s shard velocities.
#[allow(clippy::unreadable_literal)]
const FRAGMENT_VELOCITIES: [Vec2; 10] = [
    [0.16108474, 0.18276972],
    [0.008172598, 0.19987547],
    [0.27351242, 0.006472239],
    [0.19284527, 0.22853737],
    [0.18874688, 0.09974405],
    [0.091339566, 0.20896637],
    [0.0029833287, 0.26016384],
    [0.26136217, 0.11754496],
    [0.18530324, 0.21598965],
    [0.15319453, 0.20363313],
];

fn fragment(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let duration = 8.0;
    let travel = (context.progress() * duration - 0.5).clamp(0.0, duration);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        for (site, velocity) in FRAGMENT_SITES.iter().zip(FRAGMENT_VELOCITIES) {
            let shifted = [uv[0] - velocity[0] * travel, uv[1] - velocity[1] * travel];
            if shifted[0] < 0.0 || shifted[0] > 1.0 || shifted[1] < 0.0 || shifted[1] > 1.0 {
                continue;
            }
            let own = sub(shifted, *site);
            let own_distance = dot(own, own);
            let closest = FRAGMENT_SITES.iter().all(|other| {
                let other = sub(shifted, *other);
                dot(other, other) >= own_distance
            });
            if closest {
                return context.from_color([shifted[0], 1.0 - shifted[1]]);
            }
        }
        gl_to_color(context, uv)
    });
}

fn polar_function(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let segments = context.float("segments").round_ties_even();
    let size = context.size;
    transition_row(context, y, x0, out, |x, uv| {
        let outside = gl_aa_step(size, x, y, |uv| {
            let angle = atan2f(uv[1] - 0.5, uv[0] - 0.5) - 0.5 * PI;
            let radius = (cosf(segments * angle) + 4.0) / 4.0;
            (radius * progress, length(sub(uv, [0.5, 0.5])))
        });
        let uv = gl_uv(uv);
        mix4(
            gl_to_color(context, uv),
            gl_from_color(context, uv),
            outside,
        )
    });
}

/// `rectangle` and `rectangle-crop`: the coverage of the box from `t - 0.25`
/// to `1.25 - t` on both axes.
fn box_coverage(size: RenderSize, x: u32, y: u32, t: f32) -> f32 {
    let box_x = gl_aa_step(size, x, y, |uv| (t, uv[0] + 0.25))
        * gl_aa_step(size, x, y, |uv| (t, 1.25 - uv[0]));
    let box_y = gl_aa_step(size, x, y, |uv| (t, uv[1] + 0.25))
        * gl_aa_step(size, x, y, |uv| (t, 1.25 - uv[1]));
    box_x * box_y
}

fn rectangle(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let bg_color = builtin_premultiply(context.vec4("bg_color"));
    let t = (1.0 - 2.0 * progress).abs();
    transition_row(context, y, x0, out, |x, uv| {
        let coverage = box_coverage(context.size, x, y, t);
        let uv = gl_uv(uv);
        let image = if progress < 0.5 {
            gl_from_color(context, uv)
        } else {
            gl_to_color(context, uv)
        };
        mix4(image, bg_color, coverage)
    });
}

fn rectangle_crop(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let bg_color = builtin_premultiply(context.vec4("bg_color"));
    let t = 1.0 - 2.0 * (progress - 0.5).abs();
    transition_row(context, y, x0, out, |x, uv| {
        let coverage = box_coverage(context.size, x, y, t);
        let uv = gl_uv(uv);
        let image = if progress < 0.5 {
            gl_from_color(context, uv)
        } else {
            gl_to_color(context, uv)
        };
        mix4(image, bg_color, 1.0 - coverage)
    });
}

/// `star_rotate`.
fn star_rotate(v: Vec2, theta: f32) -> Vec2 {
    let cos_theta = cosf(theta);
    let sin_theta = sinf(theta);
    [
        cos_theta * v[0] - sin_theta * v[1],
        sin_theta * v[0] + cos_theta * v[1],
    ]
}

fn star_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    // 72 degrees, the shader's literal as an f32.
    let star_angle = 1.256_637_1_f32;
    let progress = context.progress();
    let thickness = context.float("border_thickness");
    let rotation = context.float("star_rotation");
    let border_color = builtin_premultiply(context.vec4("border_color"));
    let center = context.vec2("star_center");
    let scaled = (2.0 * thickness + 1.0) * progress - thickness;
    let star_value = |uv: Vec2| {
        let centered = star_rotate(sub(uv, center), rotation * star_angle);
        let theta = atan2f(centered[1], centered[0]) + PI;
        let rotated = star_rotate(centered, -star_angle * ((theta / star_angle).floor() + 0.5));
        rotated[0] * 0.3 - rotated[1].abs()
    };
    let size = context.size;
    transition_row(context, y, x0, out, |x, uv| {
        let inner = gl_aa_step(size, x, y, |uv| (-scaled, star_value(uv)));
        let outer = gl_aa_step(size, x, y, |uv| (-(scaled + thickness), star_value(uv)));
        let uv = gl_uv(uv);
        let bordered = mix4(gl_from_color(context, uv), border_color, outer);
        mix4(bordered, gl_to_color(context, uv), inner)
    });
}

/// `shape_metric`: the shape's own distance, its boundary at 1.
fn shape_metric(p: Vec2, shape: u32, aspect: f32) -> f32 {
    let a = aspect.max(0.1);
    match shape {
        1 => length([p[0] / a, p[1] / 1.0]),
        2 => p[0].abs() / a + p[1].abs(),
        3 => (p[0].abs() / a).max(p[1].abs()),
        4 => {
            let turn = atan2f(p[0], -p[1]) / core::f32::consts::TAU * 5.0;
            let tip = (fract(turn + 0.5) * 2.0 - 1.0).abs();
            length(p) / mix(1.0, 0.45, tip)
        }
        5 => (p[0].abs() * 3.0)
            .max(p[1].abs())
            .min(p[0].abs().max(p[1].abs() * 3.0)),
        _ => length(p),
    }
}

fn shape_wipe(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let shape = context.choice("shape");
    let center = context.vec2("center");
    let aspect = context.float("aspect");
    let rotation = context.float("rotation");
    let softness = context.float("softness");
    let closing = context.boolean("closing");
    let resolution = context.resolution();
    let point = |uv: Vec2| {
        let p = [
            (uv[0] - center[0]) * (resolution[0] / resolution[1]),
            (uv[1] - center[1]) * 1.0,
        ];
        let angle = rotation * PI / 180.0;
        let c = cosf(angle);
        let s = sinf(angle);
        [c * p[0] - s * p[1], s * p[0] + c * p[1]]
    };
    let corners = [
        point([0.0, 0.0]),
        point([1.0, 0.0]),
        point([0.0, 1.0]),
        point([1.0, 1.0]),
    ];
    let largest = |f: &dyn Fn(Vec2) -> f32| {
        f(corners[0])
            .max(f(corners[1]))
            .max(f(corners[2]).max(f(corners[3])))
    };
    let reach = match shape {
        4 => largest(&|c| length(c)) / 0.45,
        5 => largest(&|c| (c[0].abs() * 3.0).max(c[1].abs()))
            .min(largest(&|c| c[0].abs().max(c[1].abs() * 3.0))),
        _ => largest(&|c| shape_metric(c, shape, aspect)),
    };
    let amount = select(progress, 1.0 - progress, closing);
    let edge = amount * (reach + softness);
    transition_row(context, y, x0, out, |x, uv| {
        let footprint = fwidth_fine(context.size, x, y, |uv| {
            shape_metric(point(uv), shape, aspect)
        });
        let m = shape_metric(point(uv), shape, aspect);
        let outside = builtin_aa_ramp(edge - softness, edge, m, footprint);
        mix4(
            context.from_color(uv),
            context.to_color(uv),
            select(1.0 - outside, outside, closing),
        )
    });
}
