//! The "3D" group's kernels.

use libm::asinf;

use super::{
    KernelContext, PI, Rgba, Transition, Vec2, builtin_aa_step, builtin_interval_coverage,
    builtin_premultiply, builtin_smoothstep, cosf, dot, fract, fwidth_fine, gl_from_color,
    gl_to_color, gl_uv, mix, mix4, powf, select, sinf, transition_row,
};
use crate::cpu::kernels::{builtin_cell, builtin_hash3, builtin_over, builtin_unit, builtin_valid};

pub(super) static KERNELS: &[(&str, Transition)] = &[
    ("cube", Transition(cube)),
    ("doorway", Transition(doorway)),
    ("swap", Transition(swap)),
    ("grid-flip", Transition(grid_flip)),
    ("tiles-wave", Transition(tiles_wave)),
    ("book-flip", Transition(book_flip)),
    ("simple-flip", Transition(simple_flip)),
    ("page-peel", Transition(page_peel)),
];

#[inline]
fn scale4(color: Rgba, s: f32) -> Rgba {
    color.map(|channel| channel * s)
}

/// `fwidthFine` of each component of a vector field over the shader's uv.
fn vector_width(context: &KernelContext<'_>, x: u32, y: u32, field: impl Fn(Vec2) -> Vec2) -> Vec2 {
    [
        fwidth_fine(context.size, x, y, |uv| field(uv)[0]),
        fwidth_fine(context.size, x, y, |uv| field(uv)[1]),
    ]
}

/// `builtin_interval_coverage` of both components of `p`.
fn inside(p: Vec2, width: Vec2) -> f32 {
    builtin_interval_coverage(p[0], width[0]) * builtin_interval_coverage(p[1], width[1])
}

/// The floor reflections' projection (cube, doorway and swap).
fn project(p: Vec2, lift: f32) -> Vec2 {
    [p[0] * 1.0 + 0.0, p[1] * -1.2 + lift]
}

/// A face's reflection on the floor, faded with its height.
fn reflection(color: Rgba, reflection: f32, r: Vec2, coverage: f32) -> Rgba {
    scale4(color, reflection * (1.0 - r[1]) * coverage)
}

/// `cube.wgsl`.
fn cube(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let persp = context.float("persp");
    let unzoom = context.float("unzoom");
    let strength = context.float("reflection");
    let lift = -context.float("floating") / 100.0;
    let back = builtin_premultiply(context.vec4("back_color"));
    let skew = |p: Vec2, persp: f32, x: f32| -> Vec2 {
        [
            p[0],
            (p[1] - 0.5 * (1.0 - persp) * x) / (1.0 + (persp - 1.0) * x).max(0.0001),
        ]
    };
    let uz = unzoom * 2.0 * (0.5 - (0.5 - progress).abs());
    let from_persp = 1.0 - mix(progress, 0.0, persp);
    let to_persp = mix(powf(progress, 2.0), 1.0, persp);
    let points = |op: Vec2| -> (Vec2, Vec2) {
        let p = [
            -uz * 0.5 + (1.0 + uz) * op[0],
            -uz * 0.5 + (1.0 + uz) * op[1],
        ];
        let from_q = [(p[0] - progress) / (1.0 - progress), (p[1] - 0.0) / 1.0];
        let to_q = [p[0] / progress, p[1] / 1.0];
        (
            skew(from_q, from_persp, from_q[0]),
            skew(to_q, to_persp, 1.0 - to_q[0]),
        )
    };
    transition_row(context, y, x0, out, |x, uv| {
        let from_w = vector_width(context, x, y, |uv| points(gl_uv(uv)).0);
        let to_w = vector_width(context, x, y, |uv| points(gl_uv(uv)).1);
        let (from_p, to_p) = points(gl_uv(uv));
        let from_r = project(from_p, lift);
        let to_r = project(to_p, lift);
        let from_in = inside(from_p, from_w);
        let to_in = inside(to_p, to_w);
        let from_r_in = inside(from_r, [from_w[0], from_w[1] * 1.2]);
        let to_r_in = inside(to_r, [to_w[0], to_w[1] * 1.2]);
        let a = reflection(gl_from_color(context, from_r), strength, from_r, from_r_in);
        let b = reflection(gl_to_color(context, to_r), strength, to_r, to_r_in);
        let reflected = [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
        let floor = builtin_over(builtin_valid(reflected), back);
        let faces = mix4(
            mix4(floor, gl_to_color(context, to_p), to_in),
            gl_from_color(context, from_p),
            from_in,
        );
        builtin_valid(faces)
    });
}

/// `doorway.wgsl`.
fn doorway(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let strength = context.float("reflection");
    let perspective = context.float("perspective");
    let size = mix(1.0, context.float("depth"), 1.0 - progress);
    let back = builtin_premultiply(context.vec4("back_color"));
    let middle_slit = |p: Vec2| 2.0 * (p[0] - 0.5).abs() - progress;
    let door = |p: Vec2| -> Vec2 {
        let direction = select(1.0, -1.0, p[0] > 0.5);
        let mut door = [p[0] + direction * (0.5 * progress), p[1] + direction * 0.0];
        let d = 1.0 / (1.0 + perspective * progress * (1.0 - middle_slit(p)));
        door[1] = (door[1] - d / 2.0) * d + d / 2.0;
        door
    };
    let next = |p: Vec2| -> Vec2 { [(p[0] + -0.5) * size + 0.5, (p[1] + -0.5) * size + 0.5] };
    transition_row(context, y, x0, out, |x, uv| {
        let p = gl_uv(uv);
        let slit = middle_slit(p);
        let door_point = door(p);
        let next_point = next(p);
        let floor_point = project(next_point, -0.02);
        let door_w = vector_width(context, x, y, |uv| door(gl_uv(uv)));
        let next_w = vector_width(context, x, y, |uv| next(gl_uv(uv)));
        let slit_w = fwidth_fine(context.size, x, y, |uv| middle_slit(gl_uv(uv)) - 0.0);
        let door_in = builtin_aa_step(0.0, slit, slit_w)
            * builtin_interval_coverage(door_point[0], door_w[0])
            * builtin_interval_coverage(door_point[1], door_w[1]);
        let next_in = inside(next_point, next_w);
        let floor_in = inside(floor_point, [next_w[0], next_w[1] * 1.2]);
        let reflected = reflection(
            gl_to_color(context, floor_point),
            strength,
            floor_point,
            floor_in,
        );
        let floor = builtin_over(builtin_valid(reflected), back);
        let doors = mix4(
            mix4(floor, gl_to_color(context, next_point), next_in),
            gl_from_color(context, door_point),
            door_in,
        );
        builtin_valid(doors)
    });
}

/// `swap.wgsl`.
fn swap(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let strength = context.float("reflection");
    let perspective = context.float("perspective");
    let depth = context.float("depth");
    let back = builtin_premultiply(context.vec4("back_color"));
    let from_size = mix(1.0, depth, progress);
    let from_persp = perspective * progress;
    let to_size = mix(1.0, depth, 1.0 - progress);
    let to_persp = perspective * (1.0 - progress);
    let points = |p: Vec2| -> (Vec2, Vec2) {
        let from_p = [
            (p[0] + -0.0) * (from_size / (1.0 - perspective * progress).max(0.0001)) + 0.0,
            (p[1] + -0.5) * (from_size / (1.0 - from_size * from_persp * p[0]).max(0.0001)) + 0.5,
        ];
        let to_p = [
            (p[0] + -1.0) * (to_size / (1.0 - perspective * (1.0 - progress)).max(0.0001)) + 1.0,
            (p[1] + -0.5) * (to_size / (1.0 - to_size * to_persp * (0.5 - p[0])).max(0.0001)) + 0.5,
        ];
        (from_p, to_p)
    };
    transition_row(context, y, x0, out, |x, uv| {
        let from_w = vector_width(context, x, y, |uv| points(gl_uv(uv)).0);
        let to_w = vector_width(context, x, y, |uv| points(gl_uv(uv)).1);
        let (from_p, to_p) = points(gl_uv(uv));
        let from_r = project(from_p, -0.02);
        let to_r = project(to_p, -0.02);
        let from_in = inside(from_p, from_w);
        let to_in = inside(to_p, to_w);
        let from_r_in = inside(from_r, [from_w[0], from_w[1] * 1.2]);
        let to_r_in = inside(to_r, [to_w[0], to_w[1] * 1.2]);
        let a = reflection(gl_from_color(context, from_r), strength, from_r, from_r_in);
        let b = reflection(gl_to_color(context, to_r), strength, to_r, to_r_in);
        let reflected = [a[0] + b[0], a[1] + b[1], a[2] + b[2], a[3] + b[3]];
        let floor = builtin_over(builtin_valid(reflected), back);
        let from_card = gl_from_color(context, from_p);
        let to_card = gl_to_color(context, to_p);
        let card = if progress < 0.5 {
            mix4(mix4(floor, to_card, to_in), from_card, from_in)
        } else {
            mix4(mix4(floor, from_card, from_in), to_card, to_in)
        };
        builtin_valid(card)
    });
}

/// `grid_flip_delta` in `grid-flip.wgsl`.
fn grid_flip_delta(p: Vec2, size: Vec2) -> f32 {
    let position = [(size[0] * p[0]).floor(), (size[1] * p[1]).floor()];
    let tile = [1.0 / size[0], 1.0 / size[1]];
    let top = tile[1] * (position[1] + 1.0);
    let bottom = tile[1] * position[1];
    let left = tile[0] * position[0];
    let right = tile[0] * (position[0] + 1.0);
    let min_x = (p[0] - left).abs().min((p[0] - right).abs());
    let min_y = (p[1] - top).abs().min((p[1] - bottom).abs());
    min_x.min(min_y)
}

/// `grid-flip.wgsl`.
fn grid_flip(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let requested = context.vec2("size");
    let size = [requested[0].floor().max(1.0), requested[1].floor().max(1.0)];
    let pause = context.float("pause");
    let divider = (1.0 / size[0]).min(1.0 / size[1]) * context.float("divider_width");
    let bg = builtin_premultiply(context.vec4("bg_color"));
    let randomness = context.float("randomness");
    let flip = (progress - pause) / (1.0 - pause * 2.0);
    let tile = 1.0 / size[0];
    // The card's half width and its signed distance to the card's edge.
    let card = |p: Vec2| -> (f32, f32, f32) {
        let position = [(size[0] * p[0]).floor(), (size[1] * p[1]).floor()];
        let r = builtin_unit(builtin_hash3(
            builtin_cell(position[0]),
            builtin_cell(position[1]),
            0,
        )) - randomness;
        let cp = builtin_smoothstep(0.0, 1.0 - r, flip);
        let delta = position[0] * tile;
        let half_width = (cp - 0.5).abs();
        (
            cp,
            half_width,
            half_width - (size[0] * (p[0] - delta) - 0.5).abs(),
        )
    };
    transition_row(context, y, x0, out, |x, uv| {
        let p = gl_uv(uv);
        let delta = grid_flip_delta(p, size);
        let off_divider = if divider > 0.0 {
            let width = fwidth_fine(context.size, x, y, |uv| {
                grid_flip_delta(gl_uv(uv), size) - divider
            });
            builtin_aa_step(divider, delta, width)
        } else {
            1.0
        };
        let (cp, half_width, edge) = card(p);
        let edge_width = fwidth_fine(context.size, x, y, |uv| card(gl_uv(uv)).2);
        let on_card = builtin_aa_step(0.0, edge, edge_width);
        if progress < pause {
            let fading = progress / pause;
            return mix4(
                bg,
                gl_from_color(context, p),
                mix(1.0 - fading, 1.0, off_divider),
            );
        }
        if progress < 1.0 - pause {
            let offset = tile / 2.0 + (size[0] * p[0]).floor() * tile;
            let flipped = [
                (p[0] - offset) / half_width.max(0.0001) * 0.5 + offset,
                p[1],
            ];
            let image = if cp <= 0.5 {
                gl_from_color(context, flipped)
            } else {
                gl_to_color(context, flipped)
            };
            return mix4(bg, mix4(bg, image, on_card), off_divider);
        }
        let fading = (progress - 1.0 + pause) / pause;
        mix4(bg, gl_to_color(context, p), mix(fading, 1.0, off_divider))
    });
}

/// `tiles_wave_squeeze` in `tiles-wave.wgsl`.
fn tiles_wave_squeeze(x: f32, sin_time: f32, divisor: f32) -> f32 {
    if x < 0.5 {
        (x - sin_time) * 0.5 / divisor
    } else {
        (x - 0.5) * 0.5 / divisor + 0.5
    }
}

/// `tiles-wave.wgsl`.
fn tiles_wave(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let requested = context.vec2("tile_count");
    let count = [requested[0].floor().max(1.0), requested[1].floor().max(1.0)];
    let flips = [context.boolean("flip_x"), context.boolean("flip_y")];
    let tile_size = [1.0 / count[0], 1.0 / count[1]];
    let tiles = count[0] * count[1];
    transition_row(context, y, x0, out, |x, uv| {
        let footprint = vector_width(context, x, y, |uv| {
            let uv = gl_uv(uv);
            [uv[0] * count[0], uv[1] * count[1]]
        });
        let uv = gl_uv(uv);
        let in_tile = [fract(uv[0] * count[0]), fract(uv[1] * count[1])];
        let tile = [(uv[0] * count[0]).floor(), (uv[1] * count[1]).floor()];
        let offset = (tile[1] + tile[0] * count[1]) / tiles;
        let time_offset = ((progress - offset) * tiles).clamp(0.0, 0.5);
        let sin_time = 1.0 - cosf(fract(time_offset) * PI).abs();
        let outgoing = sin_time <= 0.5;
        let half_card = (0.5 - sin_time).abs();
        let divisor = select(0.5 - sin_time, 0.0001, (0.5 - sin_time).abs() < 0.0001);
        let mut coverage = 1.0;
        let mut global_uv = [0.0; 2];
        for axis in 0..2 {
            let mut card_point = in_tile[axis];
            if flips[axis] {
                let cover = (0.5
                    + (half_card - (in_tile[axis] - 0.5).abs()) / footprint[axis].max(0.000_000_1))
                .clamp(0.0, 1.0);
                coverage *= cover;
                let squeezed = tiles_wave_squeeze(in_tile[axis], sin_time, divisor);
                card_point = select(1.0 - squeezed, squeezed, outgoing);
            }
            global_uv[axis] = tile[axis] * tile_size[axis] + card_point * tile_size[axis];
        }
        let (card, around) = if outgoing {
            (
                gl_from_color(context, global_uv),
                gl_from_color(context, uv),
            )
        } else {
            (gl_to_color(context, global_uv), gl_to_color(context, uv))
        };
        mix4(around, card, coverage)
    });
}

/// `book_flip_divisor` in `book-flip.wgsl`.
fn book_flip_divisor(value: f32) -> f32 {
    select(value, 0.0001, value.abs() < 0.0001)
}

/// `book-flip.wgsl`.
fn book_flip(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let shade = (progress - 0.5).abs() * 2.0;
    let shade = shade.max(0.7);
    let shaded = |color: Rgba| {
        [
            color[0] * shade,
            color[1] * shade,
            color[2] * shade,
            color[3],
        ]
    };
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| gl_uv(uv)[0] - (1.0 - progress));
        let p = gl_uv(uv);
        let turned = builtin_aa_step(1.0 - progress, p[0], width);
        if p[0] < 0.5 {
            let skew_x = (p[0] - 0.5) / book_flip_divisor(progress - 0.5) * 0.5 + 0.5;
            let skew_y = (p[1] - 0.5) / (0.5 + (1.0 - progress) * (0.5 - p[0]) / 0.5) * 0.5 + 0.5;
            mix4(
                gl_from_color(context, p),
                shaded(gl_to_color(context, [skew_x, skew_y])),
                turned,
            )
        } else {
            let skew_x = (p[0] - progress) / book_flip_divisor(0.5 - progress) * 0.5;
            let skew_y = (p[1] - 0.5) / (0.5 + progress * (p[0] - 0.5) / 0.5) * 0.5 + 0.5;
            mix4(
                shaded(gl_from_color(context, [skew_x, skew_y])),
                gl_to_color(context, p),
                turned,
            )
        }
    });
}

/// `simple-flip.wgsl`.
fn simple_flip(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let back = builtin_premultiply(context.vec4("back_color"));
    let half_width = (progress - 0.5).abs();
    transition_row(context, y, x0, out, |x, uv| {
        let width = fwidth_fine(context.size, x, y, |uv| {
            half_width - (gl_uv(uv)[0] - 0.5).abs()
        });
        let uv = gl_uv(uv);
        let card_uv = [(uv[0] - 0.5) / half_width.max(0.0001) * 0.5 + 0.5, uv[1]];
        let card = if progress >= 0.5 {
            gl_to_color(context, card_uv)
        } else {
            gl_from_color(context, card_uv)
        };
        let coverage = builtin_aa_step((uv[0] - 0.5).abs(), half_width, width);
        mix4(back, card, coverage)
    });
}

/// `page-peel.wgsl`.
fn page_peel(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let resolution = context.resolution();
    let aspect = [resolution[0] / resolution[1], 1.0];
    let angle = context.float("angle") * (PI / 180.0);
    let d = [cosf(angle), -sinf(angle)];
    let radius = context.float("radius").max(0.0001);
    let back_shade = context.float("back_shade");
    let half_turn = PI * radius;
    let c0 = dot([0.0, 0.0], d);
    let c1 = dot([aspect[0], 0.0], d);
    let c2 = dot([0.0, 1.0], d);
    let c3 = dot([aspect[0], 1.0], d);
    let start = c0.min(c1).min(c2.min(c3));
    let end = c0.max(c1).max(c2.max(c3)) + radius;
    let fold = mix(start, end, progress);
    // The signed distance ahead of the fold, the turn on the cylinder, and
    // the sheet points seen face up and face down.
    let sheet = |uv: Vec2| -> (f32, f32, Vec2, Vec2) {
        let p = [uv[0] * aspect[0], uv[1] * aspect[1]];
        let x = dot(p, d) - fold;
        let turn = asinf((-x / radius).clamp(0.0, 1.0));
        let front_arc = select(-x, radius * turn, x < 0.0);
        let back_arc = select(x + half_turn, radius * (PI - turn), x < 0.0);
        let front_uv = [
            (p[0] - d[0] * (x + front_arc)) / aspect[0],
            (p[1] - d[1] * (x + front_arc)) / aspect[1],
        ];
        let back_uv = [
            (p[0] - d[0] * (x + back_arc)) / aspect[0],
            (p[1] - d[1] * (x + back_arc)) / aspect[1],
        ];
        (x, turn, front_uv, back_uv)
    };
    transition_row(context, y, x0, out, |px, uv| {
        let (x, turn, front_uv, back_uv) = sheet(uv);
        let front_w = vector_width(context, px, y, |uv| sheet(uv).2);
        let back_w = vector_width(context, px, y, |uv| sheet(uv).3);
        let front_in = inside(front_uv, front_w);
        let back_in = inside(back_uv, back_w);
        let sheet_width = fwidth_fine(context.size, px, y, |uv| sheet(uv).0 - -radius);
        let on_sheet = builtin_aa_step(-radius, x, sheet_width);
        let light = select(1.0, 0.6 + 0.4 * cosf(turn).abs(), x < 0.0);
        let front = context.from_color(front_uv);
        let back = context.from_color(back_uv);
        let whitened = mix4(back, [back[3]; 4], back_shade);
        let incoming = context.to_color(uv);
        let face_up = mix4(
            incoming,
            [
                front[0] * light,
                front[1] * light,
                front[2] * light,
                front[3],
            ],
            front_in,
        );
        let layered = mix4(
            face_up,
            [
                whitened[0] * light,
                whitened[1] * light,
                whitened[2] * light,
                whitened[3],
            ],
            back_in,
        );
        mix4(incoming, layered, on_sheet)
    });
}
