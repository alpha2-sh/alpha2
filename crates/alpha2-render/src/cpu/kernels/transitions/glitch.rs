//! The "Glitch" group's kernels.

use libm::exp2f;

use super::{
    KernelContext, PI, Rgba, Transition, Vec2, builtin_smoothstep, builtin_straight, cosf, fract,
    gl_from_color, gl_to_color, gl_uv, mix, mix4, powf, select, sinf, step, transition_row,
};
use crate::cpu::kernels::{builtin_cell, builtin_hash, builtin_hash3, builtin_unit, builtin_valid};

pub(super) static KERNELS: &[(&str, Transition)] = &[
    ("strip-datamosh-glitch", Transition(strip_datamosh_glitch)),
    ("drop-zone-flicker", Transition(drop_zone_flicker)),
    ("glitch-displace", Transition(glitch_displace)),
    ("old-tv-lost-signal", Transition(old_tv_lost_signal)),
    ("static-fade", Transition(static_fade)),
    ("tv-static", Transition(tv_static)),
    ("parametric-glitch", Transition(parametric_glitch)),
    ("glitch", Transition(glitch)),
];

/// A value in `[0, 1)` for two integer-valued seeds (`strip_hash`,
/// `drop_zone_hash`).
fn seed_hash(a: f32, b: f32, salt: u32) -> f32 {
    builtin_unit(builtin_hash3(builtin_cell(a), builtin_cell(b), salt))
}

fn clamp_uv(uv: Vec2, low: f32, high: f32) -> Vec2 {
    [uv[0].clamp(low, high), uv[1].clamp(low, high)]
}

/// The channels of three taps; the alpha covers all three.
fn chroma(red: Rgba, green: Rgba, blue: Rgba) -> Rgba {
    [red[0], green[1], blue[2], green[3].max(red[3].max(blue[3]))]
}

/// `strip_y` in `strip-datamosh-glitch.wgsl`.
fn strip_y(uv: Vec2, density: f32, seed: f32, min_width: f32, max_width: f32) -> f32 {
    let y = uv[1] * density + seed * 0.137;
    let id = y.floor();
    let f = fract(y);
    let c = seed_hash(id, seed, 1);
    let w = mix(min_width, max_width, seed_hash(id, seed, 2));
    1.0 - builtin_smoothstep(w, w + 0.018, (f - c).abs())
}

/// `strip_x` in `strip-datamosh-glitch.wgsl`.
fn strip_x(uv: Vec2, density: f32, seed: f32, min_width: f32, max_width: f32) -> f32 {
    let x = uv[0] * density + seed * 0.091;
    let id = x.floor();
    let f = fract(x);
    let c = seed_hash(id, seed, 3);
    let w = mix(min_width, max_width, seed_hash(id, seed, 4));
    1.0 - builtin_smoothstep(w, w + 0.012, (f - c).abs())
}

fn strip_broken_gate(uv: Vec2, row: f32, row_salt: u32, frame: f32) -> f32 {
    let segments = mix(1.0, 9.0, seed_hash(row, frame, 5));
    let segment = (uv[0] * segments).floor();
    step(0.16, seed_hash(segment, row + frame * 3.0, row_salt))
}

fn strip_horizontal_mask(uv: Vec2, frame: f32, bars: f32) -> f32 {
    let r1 = ((uv[1] + seed_hash(frame, 0.0, 6) * 0.031) * bars * 0.38).floor();
    let r2 = ((uv[1] + seed_hash(frame, 0.0, 7) * 0.013) * bars).floor();
    let r3 = ((uv[1] + seed_hash(frame, 0.0, 8) * 0.006) * bars * 3.4).floor();
    let mut thick = strip_y(uv, bars * 0.38, frame + 1.0, 0.035, 0.22);
    let mut mid = strip_y(uv, bars, frame + 4.0, 0.014, 0.11);
    let mut hair = strip_y(uv, bars * 3.4, frame + 9.0, 0.004, 0.035);
    thick *= step(0.42, seed_hash(r1, frame, 10));
    mid *= step(0.48, seed_hash(r2, frame, 11));
    hair *= step(0.62, seed_hash(r3, frame, 12));
    thick *= strip_broken_gate(
        uv,
        r1,
        builtin_hash3(builtin_cell(r1), builtin_cell(frame), 13),
        frame,
    );
    mid *= strip_broken_gate(
        uv,
        r2,
        builtin_hash3(builtin_cell(r2), builtin_cell(frame), 14),
        frame + 3.0,
    );
    thick.max(mid.max(hair)).clamp(0.0, 1.0)
}

fn strip_vertical_mask(uv: Vec2, frame: f32, slits: f32) -> f32 {
    let column = ((uv[0] + seed_hash(frame, 0.0, 15) * 0.017) * slits).floor();
    let slit = strip_x(uv, slits, frame + 13.0, 0.01, 0.075);
    (slit * step(0.66, seed_hash(column, frame, 16))).clamp(0.0, 1.0)
}

/// `strip_chroma_from` (`sign` 1) and `strip_chroma_to` (`sign` -1) in
/// `strip-datamosh-glitch.wgsl`.
fn strip_chroma(context: &KernelContext<'_>, incoming: bool, uv: Vec2, s: Vec2) -> Rgba {
    let sample = |uv: Vec2| {
        if incoming {
            gl_to_color(context, uv)
        } else {
            gl_from_color(context, uv)
        }
    };
    let p = clamp_uv(uv, 0.0, 1.0);
    let plus = sample(clamp_uv([p[0] + s[0], p[1] + s[1]], 0.0, 1.0));
    let green = sample(p);
    let minus = sample(clamp_uv([p[0] - s[0], p[1] - s[1]], 0.0, 1.0));
    if incoming {
        chroma(minus, green, plus)
    } else {
        chroma(plus, green, minus)
    }
}

/// `strip-datamosh-glitch.wgsl`.
#[allow(clippy::many_single_char_names)]
fn strip_datamosh_glitch(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let strength = context.float("strength");
    let bars = context.float("horizontal_bars");
    let slits = context.float("vertical_slits");
    let tear = context.float("tear");
    let chroma_amount = context.float("chroma");
    let residue = context.float("residue");
    let noise_amount = context.float("noise_amount");
    let scan_amount = context.float("scan_amount");
    let flash_amount = context.float("flash_amount");
    let ratio = context.ratio();
    let b = powf(0.0_f32.max(sinf(progress * PI)), 0.42) * strength;
    let frame = (progress * 30.0).floor();
    let strobe = step(0.78, seed_hash(frame, 0.0, 24)) * powf(b, 1.65);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let h = strip_horizontal_mask(uv, frame, bars);
        let v = strip_vertical_mask(uv, frame, slits);
        let glitch = h.max(v * 0.75).clamp(0.0, 1.0);
        let row = (uv[1] * bars).floor();
        let column = (uv[0] * slits).floor();
        let row_random = seed_hash(row, frame, 17);
        let band_delay = (row_random - 0.5) * 0.30 * h;
        let reveal = builtin_smoothstep(0.18, 0.84, progress + band_delay);
        let split = [
            chroma_amount * b * (1.0 + 1.7 * glitch),
            chroma_amount * 0.22 * b * v,
        ];
        let tear_random = seed_hash(row, frame, 18);
        let drag_random = seed_hash(column, frame, 19);
        let x_tear = (tear_random - 0.5) * 2.0 * tear * b * h
            + sinf(uv[1] * 120.0 + progress * 95.0) * 0.006 * b;
        let y_drag = (drag_random - 0.5) * 0.13 * b * v;
        let micro = (seed_hash(row, column + frame, 20) - 0.5) * 0.018 * b * h.max(v);
        let from_uv = [uv[0] + (x_tear + micro), uv[1] + y_drag];
        let to_uv = [uv[0] + (-x_tear + micro), uv[1] + y_drag];
        let mut color = mix4(
            strip_chroma(context, false, from_uv, split),
            strip_chroma(context, true, to_uv, split),
            reveal,
        );
        let mut smear_uv = uv;
        smear_uv[0] += (row_random - 0.5) * 0.46 * b * h;
        smear_uv[1] += (seed_hash(row, frame, 21) - 0.5) * 0.045 * b * h;
        let slice_reveal = builtin_smoothstep(0.28, 0.78, progress + (row_random - 0.5) * 0.22);
        let wide_split = [split[0] * 1.65, split[1] * 1.65];
        let slice_color = mix4(
            strip_chroma(context, false, smear_uv, wide_split),
            strip_chroma(
                context,
                true,
                [
                    smear_uv[0] - (row_random - 0.5) * 0.18 * b,
                    smear_uv[1] - 0.0,
                ],
                wide_split,
            ),
            slice_reveal,
        );
        color = mix4(color, slice_color, h * b * residue);
        let hair_line = strip_y(uv, 190.0, frame + 55.0, 0.002, 0.012)
            * step(0.70, seed_hash((uv[1] * 190.0).floor(), frame, 22));
        let spark = hair_line * b * 0.28 * color[3];
        let mut rgb = [
            color[0] + 0.72 * spark,
            color[1] + 0.90 * spark,
            color[2] + 1.0 * spark,
        ];
        let scan = 0.5 + 0.5 * sinf(uv[1] * 980.0 + progress * 130.0);
        let darken = 1.0 - scan_amount * b * scan;
        rgb = rgb.map(|channel| channel * darken);
        let cell = [(uv[0] * (360.0 * ratio)).floor(), (uv[1] * 210.0).floor()];
        let n = seed_hash(cell[0] + frame * 7.0, cell[1] + frame * 13.0, 23);
        let noise = (n - 0.5) * noise_amount * b * (0.55 + glitch) * color[3];
        rgb = rgb.map(|channel| channel + noise);
        let luma = rgb[0] * 0.299 + rgb[1] * 0.587 + rgb[2] * 0.114;
        rgb = rgb.map(|channel| mix(channel, luma, 0.18 * b * glitch));
        let flash = strobe * flash_amount * color[3];
        rgb = rgb.map(|channel| channel + flash);
        builtin_valid([rgb[0], rgb[1], rgb[2], color[3]])
    });
}

/// `drop_zone_reveal` in `drop-zone-flicker.wgsl`.
fn drop_zone_reveal(f: f32) -> f32 {
    const STEPS: [f32; 23] = [
        0.00, 0.28, 0.43, 0.46, 0.38, 0.48, 0.26, 0.10, 0.00, 0.00, 0.30, 0.58, 1.00, 0.70, 0.42,
        0.55, 0.72, 0.88, 0.34, 0.48, 0.56, 0.76, 0.93,
    ];
    STEPS
        .iter()
        .enumerate()
        .find(|(index, _)| f < *index as f32 + 0.5)
        .map_or(1.0, |(_, value)| *value)
}

/// `drop_zone_glitch` in `drop-zone-flicker.wgsl`.
fn drop_zone_glitch(f: f32) -> f32 {
    const STEPS: [(f32, f32); 8] = [
        (0.5, 0.00),
        (6.5, 0.92),
        (10.5, 0.38),
        (11.5, 0.78),
        (12.5, 0.12),
        (17.5, 0.74),
        (22.5, 0.88),
        (23.5, 0.28),
    ];
    STEPS
        .iter()
        .find(|(below, _)| f < *below)
        .map_or(0.0, |(_, value)| *value)
}

fn drop_zone_safe(uv: Vec2) -> Vec2 {
    clamp_uv(uv, 0.001, 0.999)
}

fn drop_zone_chroma(context: &KernelContext<'_>, incoming: bool, uv: Vec2, amount: f32) -> Rgba {
    let sample = |uv: Vec2| {
        let uv = drop_zone_safe(uv);
        if incoming {
            gl_to_color(context, uv)
        } else {
            gl_from_color(context, uv)
        }
    };
    let plus = sample([uv[0] + amount, uv[1] + 0.0]);
    let green = sample(uv);
    let minus = sample([uv[0] - amount, uv[1] - 0.0]);
    if incoming {
        chroma(minus, green, plus)
    } else {
        chroma(plus, green, minus)
    }
}

fn drop_zone_blocks(uv: Vec2, f: f32) -> f32 {
    let big_x = (uv[0] * 4.0).floor();
    let small = [(uv[0] * 8.0).floor(), (uv[1] * 4.0).floor()];
    let wide = step(0.48, seed_hash(big_x, f, 1));
    let chunks = step(0.56, seed_hash(small[0] + small[1] * 8.0, f, 2));
    let cut = builtin_smoothstep(-0.035, 0.035, uv[0] - mix(0.18, 0.78, seed_hash(f, 0.0, 3)));
    (mix(wide, chunks, 0.38) * 0.72 + cut * 0.28).clamp(0.0, 1.0)
}

/// `drop-zone-flicker.wgsl`.
fn drop_zone_flicker(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let block_amount = context.float("block_amount");
    let ghost_amount = context.float("ghost_amount");
    let red_cyan = context.float("red_cyan");
    let scanline = context.float("scanline");
    let f = (progress * context.float("frame_rate")).floor();
    let reveal = drop_zone_reveal(f);
    let glitch = drop_zone_glitch(f);
    let offset = context.float("rgb_offset") * glitch;
    let jitter = [
        (seed_hash(f, 0.0, 4) - 0.5) * 0.042 * glitch,
        (seed_hash(f, 0.0, 5) - 0.5) * 0.010 * glitch,
    ];
    let fading_return = glitch * (1.0 - builtin_smoothstep(0.94, 1.0, reveal));
    let exposure = (seed_hash(f, 0.0, 7) - 0.35) * 0.10 * glitch;
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let from_uv = drop_zone_safe([uv[0] + jitter[0], uv[1] + jitter[1]]);
        let to_uv = drop_zone_safe([uv[0] - jitter[0] * 0.55, uv[1] - jitter[1] * 0.55]);
        let block = drop_zone_blocks(uv, f);
        let local_reveal = (reveal
            + (block - 0.5) * block_amount * glitch
            + (seed_hash(f, (uv[1] * 9.0).floor(), 6) - 0.5) * 0.18 * glitch)
            .clamp(0.0, 1.0);
        let local_reveal = builtin_smoothstep(0.22, 0.78, local_reveal);
        let old_clip = drop_zone_chroma(context, false, from_uv, offset);
        let new_clip = drop_zone_chroma(context, true, to_uv, offset);
        let mut color = mix4(old_clip, new_clip, local_reveal);
        let left_wash = (1.0 - builtin_smoothstep(0.10, 0.78, uv[0])) * glitch;
        let cyan_wash = builtin_smoothstep(0.08, 0.62, uv[0])
            * (1.0 - builtin_smoothstep(0.86, 1.0, uv[0]))
            * glitch;
        let red_ghost = [
            old_clip[0] * 1.34,
            old_clip[1] * 0.42,
            old_clip[2] * 0.38,
            old_clip[3],
        ];
        let cyan_ghost = [
            new_clip[0] * 0.48,
            new_clip[1] * 1.12,
            new_clip[2] * 1.24,
            new_clip[3],
        ];
        color = mix4(color, red_ghost, left_wash * red_cyan * 0.42);
        color = mix4(color, cyan_ghost, cyan_wash * red_cyan * 0.30);
        let old_return = fading_return * (0.18 + 0.52 * (1.0 - (local_reveal - 0.5).abs() * 2.0));
        color = mix4(color, old_clip, old_return * ghost_amount);
        let lines = sinf((uv[1] + f * 0.017) * 1080.0);
        let light = (lines * scanline * glitch + exposure) * color[3];
        builtin_valid([
            color[0] + light,
            color[1] + light,
            color[2] + light,
            color[3],
        ])
    });
}

/// `glitch_displace_offset` in `glitch-displace.wgsl`.
fn glitch_displace_offset(
    tex: Rgba,
    uv: Vec2,
    dot_depth: f32,
    texture_depth: f32,
    strength: f32,
) -> Vec2 {
    let dis = |channel: f32| channel * dot_depth + 1.0 - channel * texture_depth;
    let dis_x = dis(tex[0]) - 1.0 + texture_depth * dot_depth;
    let dis_y = dis(tex[1]) - 1.0 + texture_depth * dot_depth;
    [uv[0] + dis_x * strength, uv[1] + dis_y * strength]
}

#[allow(clippy::float_cmp)]
fn glitch_displace_ease1(t: f32) -> f32 {
    if t == 0.0 || t == 1.0 {
        return t;
    }
    if t < 0.5 {
        return 0.5 * exp2f(20.0 * t - 10.0);
    }
    -0.5 * exp2f(10.0 - t * 20.0) + 1.0
}

#[allow(clippy::float_cmp)]
fn glitch_displace_ease2(t: f32) -> f32 {
    if t == 1.0 {
        return t;
    }
    1.0 - exp2f(-10.0 * t)
}

/// `glitch-displace.wgsl`.
fn glitch_displace(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let val = glitch_displace_ease1(progress);
    let ease = glitch_displace_ease2(progress);
    let from_amount = builtin_smoothstep(0.0, 0.5, progress);
    let to_amount = builtin_smoothstep(1.0, 0.5, progress);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let color1 = gl_from_color(context, uv);
        let color2 = gl_to_color(context, uv);
        let disp = glitch_displace_offset(color1, uv, 0.33, 0.7, 1.0 - val);
        let disp2 = glitch_displace_offset(color2, uv, 0.33, 0.5, ease);
        let displaced1 = gl_to_color(context, disp);
        let displaced2 = gl_from_color(context, disp2);
        let low: Rgba = core::array::from_fn(|i| displaced2[i].min(displaced1[i]));
        let gray = low[0] * 0.299 + low[1] * 0.587 + low[2] * 0.114;
        let alpha = displaced1[3].max(displaced2[3]);
        let flash_rgb = (gray * 2.0).min(alpha);
        let flash = [flash_rgb, flash_rgb, flash_rgb, alpha];
        let color1 = mix4(color1, flash, from_amount);
        let color2 = mix4(color2, displaced1, to_amount);
        builtin_valid(mix4(color1, color2, val))
    });
}

/// `old-tv-lost-signal.wgsl`.
fn old_tv_lost_signal(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let p = context.progress();
    let strength = sinf(p * PI);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let mut color = mix4(gl_from_color(context, uv), gl_to_color(context, uv), p);
        let line_y = (uv[1] * 120.0).floor();
        let noise = builtin_unit(builtin_hash3(builtin_cell(line_y), p.to_bits(), 0));
        let line = step(0.92, noise);
        let drift = sinf(uv[1] * 30.0 + p * 10.0) * 0.02 * strength;
        let shifted = [uv[0] + drift, uv[1] + 0.0];
        let line_color = mix4(
            gl_from_color(context, shifted),
            gl_to_color(context, shifted),
            p,
        );
        color = mix4(color, line_color, line * strength);
        let scan = sinf(uv[1] * 900.0) * 0.03;
        let darken = scan * strength * color[3];
        builtin_valid([
            color[0] - darken,
            color[1] - darken,
            color[2] - darken,
            color[3],
        ])
    });
}

/// `static-fade.wgsl`.
fn static_fade(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let cells = context.float("n_noise_pixels");
    let luminosity = context.float("static_luminosity");
    let frame = builtin_hash(progress.to_bits());
    let transition_progress = (2.0 * (progress - 0.5)).abs();
    let intensity = (1.2 * (1.0 - transition_progress) - 0.1).min(1.0);
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let cell_x = builtin_cell((uv[0] * cells).floor());
        let cell_y = builtin_cell((uv[1] * cells).floor());
        if builtin_unit(builtin_hash3(cell_x, cell_y, 0)) <= intensity {
            let channel = |salt: u32| {
                luminosity * builtin_unit(builtin_hash3(cell_x, cell_y, frame.wrapping_add(salt)))
            };
            return [channel(1), channel(2), channel(3), 1.0];
        }
        if progress >= 0.5 {
            gl_to_color(context, uv)
        } else {
            gl_from_color(context, uv)
        }
    });
}

/// `tv-static.wgsl`.
fn tv_static(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let offset = context.float("offset");
    let resolution = context.resolution();
    transition_row(context, y, x0, out, |_, uv| {
        let p = gl_uv(uv);
        if progress < offset {
            return gl_from_color(context, p);
        }
        if progress > 1.0 - offset {
            return gl_to_color(context, p);
        }
        let pixel = [
            (p[0] * resolution[0]).floor(),
            (p[1] * resolution[1]).floor(),
        ];
        let noise = builtin_unit(builtin_hash3(
            builtin_cell(pixel[0]),
            builtin_cell(pixel[1]),
            progress.to_bits(),
        ));
        [noise, noise, noise, 1.0]
    });
}

/// `parametric-glitch.wgsl`.
fn parametric_glitch(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let amp_x = context.float("amp_x");
    let amp_y = context.float("amp_y");
    transition_row(context, y, x0, out, |_, uv| {
        let uv = gl_uv(uv);
        let rgb = builtin_straight(gl_from_color(context, uv));
        let incoming = gl_to_color(context, uv);
        let sphere = rgb[0] * rgb[0] + rgb[1] * rgb[1] + rgb[2] * rgb[2] - 1.0;
        let spiral_x = cosf(sphere - uv[0] / (progress + 0.01));
        let spiral_y = sinf(sphere - uv[1] / (progress + 0.01));
        let st = [
            fract(amp_x * uv[0] * spiral_x),
            fract(amp_y * uv[1] * spiral_y),
        ];
        let diff = [uv[0] - st[0], uv[1] - st[1]];
        mix4(
            gl_from_color(
                context,
                [uv[0] + progress * diff[0], uv[1] + progress * diff[1]],
            ),
            incoming,
            progress,
        )
    });
}

/// `glitch.wgsl`, in the top-left uv.
fn glitch(context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [Rgba]) {
    let progress = context.progress();
    let amount = context.float("amount");
    let block_size = context.float("block_size");
    let seed = context
        .float("seed")
        .round_ties_even()
        .clamp(0.0, 16_777_215.0) as u32;
    let strength = sinf(PI * progress) * amount;
    let stage = (progress * 24.0).floor() as u32;
    transition_row(context, y, x0, out, |_, uv| {
        let row = builtin_cell(uv[1] / block_size);
        let h = builtin_hash3(row, stage, seed);
        let jumping = select(0.0, 1.0, builtin_unit(builtin_hash(h)) < strength);
        let shift = (builtin_unit(h) - 0.5) * 0.25 * strength * jumping;
        let split = 0.015 * strength * jumping;
        let switched = builtin_unit(builtin_hash3(row, 4096, seed)) < progress;
        let p = [uv[0] + shift, uv[1] + 0.0];
        let sample = |p: Vec2| {
            if switched {
                context.to_color(p)
            } else {
                context.from_color(p)
            }
        };
        let base = sample(p);
        let red = sample([p[0] + split, p[1] + 0.0]);
        let blue = sample([p[0] - split, p[1] - 0.0]);
        [red[0], base[1], blue[2], base[3].max(red[3].max(blue[3]))]
    });
}
