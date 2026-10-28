//! WGSL built-ins and the shared WGSL helpers kernels port: the
//! transitions' `helpers.wgsl` and `gl-entry.wgsl`, and fine derivatives
//! emulated on 2 x 2 quads exactly as WGSL defines them.

use super::KernelContext;
use crate::RenderSize;

/// The shader's `uv` at the centre of pixel `(x, y)`: `(0, 0)` top-left.
#[inline]
#[allow(clippy::cast_precision_loss)]
pub(crate) fn pixel_uv(size: RenderSize, x: u32, y: u32) -> [f32; 2] {
    [
        (x as f32 + 0.5) / size.width as f32,
        (y as f32 + 0.5) / size.height as f32,
    ]
}

/// Fills `out` with `shade(x, uv)` for the pixels `x0..` of row `y`.
#[inline]
pub(crate) fn shade_row(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
    shade: impl Fn(u32, [f32; 2]) -> [f32; 4],
) {
    for (offset, pixel) in out.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let x = x0 + offset as u32;
        *pixel = shade(x, pixel_uv(context.size, x, y));
    }
}

/// `dpdxFine(field)` at pixel `(x, y)`: the field at the right pixel of
/// its 2 x 2 quad's row minus the field at the left one.
#[inline]
pub(crate) fn dpdx_fine(size: RenderSize, x: u32, y: u32, field: impl Fn([f32; 2]) -> f32) -> f32 {
    let left = x & !1;
    field(pixel_uv(size, left + 1, y)) - field(pixel_uv(size, left, y))
}

/// `dpdyFine(field)` at pixel `(x, y)`.
#[inline]
pub(crate) fn dpdy_fine(size: RenderSize, x: u32, y: u32, field: impl Fn([f32; 2]) -> f32) -> f32 {
    let top = y & !1;
    field(pixel_uv(size, x, top + 1)) - field(pixel_uv(size, x, top))
}

/// `fwidthFine(field)` at pixel `(x, y)`.
#[inline]
pub(crate) fn fwidth_fine(
    size: RenderSize,
    x: u32,
    y: u32,
    field: impl Fn([f32; 2]) -> f32,
) -> f32 {
    dpdx_fine(size, x, y, &field).abs() + dpdy_fine(size, x, y, &field).abs()
}

#[inline]
pub(crate) fn mix(a: f32, b: f32, t: f32) -> f32 {
    a * (1.0 - t) + b * t
}

#[inline]
pub(crate) fn mix4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    [
        mix(a[0], b[0], t),
        mix(a[1], b[1], t),
        mix(a[2], b[2], t),
        mix(a[3], b[3], t),
    ]
}

/// WGSL `step(edge, x)`.
#[inline]
pub(crate) fn step(edge: f32, x: f32) -> f32 {
    if x >= edge { 1.0 } else { 0.0 }
}

/// WGSL `fract`.
#[inline]
pub(crate) fn fract(x: f32) -> f32 {
    x - x.floor()
}

/// WGSL `smoothstep` with ascending edges.
#[inline]
pub(crate) fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// `builtin_smoothstep`: hard edges and descending ramps handled.
#[inline]
#[allow(clippy::float_cmp)]
pub(crate) fn builtin_smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge0 == edge1 {
        return step(edge0, x);
    }
    smoothstep(edge0, edge1, x)
}

/// `builtin_smoothstep4`.
#[inline]
pub(crate) fn builtin_smoothstep4(edge0: [f32; 4], edge1: [f32; 4], x: f32) -> [f32; 4] {
    [
        builtin_smoothstep(edge0[0], edge1[0], x),
        builtin_smoothstep(edge0[1], edge1[1], x),
        builtin_smoothstep(edge0[2], edge1[2], x),
        builtin_smoothstep(edge0[3], edge1[3], x),
    ]
}

/// `builtin_straight`.
#[inline]
pub(crate) fn builtin_straight(color: [f32; 4]) -> [f32; 3] {
    if color[3] <= 0.000_001 {
        return [0.0; 3];
    }
    [
        color[0] / color[3],
        color[1] / color[3],
        color[2] / color[3],
    ]
}

/// `builtin_premultiply`.
#[inline]
pub(crate) fn builtin_premultiply(color: [f32; 4]) -> [f32; 4] {
    [
        color[0] * color[3],
        color[1] * color[3],
        color[2] * color[3],
        color[3],
    ]
}

/// `builtin_aa_step(edge, value)`, given `width`, the `fwidthFine` of
/// `value - edge`.
#[inline]
pub(crate) fn builtin_aa_step(edge: f32, value: f32, width: f32) -> f32 {
    let signed_distance = value - edge;
    if width <= 0.000_000_1 {
        return step(edge, value);
    }
    (0.5 + signed_distance / width).clamp(0.0, 1.0)
}

/// `builtin_aa_ramp`.
#[inline]
pub(crate) fn builtin_aa_ramp(edge0: f32, edge1: f32, value: f32, footprint: f32) -> f32 {
    let width = footprint.max(0.000_000_1);
    // The shader's arithmetic, not `midpoint`'s.
    #[allow(clippy::manual_midpoint)]
    let center = (edge0 + edge1) * 0.5;
    let half_span = (edge1 - edge0).abs().max(width) * 0.5;
    let ramp = builtin_smoothstep(center - half_span, center + half_span, value);
    if edge1 < edge0 { 1.0 - ramp } else { ramp }
}

/// `builtin_aa_smoothstep(edge0, edge1, value)`, given `width`, the
/// `fwidthFine` of `value`.
#[inline]
pub(crate) fn builtin_aa_smoothstep(edge0: f32, edge1: f32, value: f32, width: f32) -> f32 {
    builtin_aa_ramp(edge0, edge1, value, width)
}

/// `builtin_pulse_integral`.
#[inline]
pub(crate) fn builtin_pulse_integral(phase: f32, duty: f32) -> f32 {
    phase.floor() * duty + fract(phase).min(duty)
}

/// `builtin_aa_pulse`.
#[inline]
pub(crate) fn builtin_aa_pulse(phase: f32, duty: f32, width: f32) -> f32 {
    let span = width.max(0.000_000_1);
    let coverage = (builtin_pulse_integral(phase + span * 0.5, duty)
        - builtin_pulse_integral(phase - span * 0.5, duty))
        / span;
    coverage.clamp(0.0, 1.0)
}

/// `builtin_angle_width`.
#[inline]
pub(crate) fn builtin_angle_width(centered: [f32; 2], resolution: [f32; 2]) -> f32 {
    let squared_radius = (centered[0] * centered[0] + centered[1] * centered[1]).max(0.000_000_1);
    (centered[1].abs() * (1.0 / resolution[0]) + centered[0].abs() * (1.0 / resolution[1]))
        / squared_radius
}

/// `builtin_interval_coverage`.
#[inline]
pub(crate) fn builtin_interval_coverage(value: f32, width: f32) -> f32 {
    let span = width.max(0.000_000_1);
    (((value + span * 0.5).min(1.0) - (value - span * 0.5).max(0.0)) / span).clamp(0.0, 1.0)
}

/// The gl-transitions coordinates of a shader `uv`: y up.
#[inline]
pub(crate) const fn gl_uv(uv: [f32; 2]) -> [f32; 2] {
    [uv[0], 1.0 - uv[1]]
}

/// `gl_from_color(uv)`, `uv` in gl-transitions coordinates.
#[inline]
pub(crate) fn gl_from_color(context: &KernelContext<'_>, uv: [f32; 2]) -> [f32; 4] {
    context.from_color(gl_uv(uv))
}

/// `gl_to_color(uv)`, `uv` in gl-transitions coordinates.
#[inline]
pub(crate) fn gl_to_color(context: &KernelContext<'_>, uv: [f32; 2]) -> [f32; 4] {
    context.to_color(gl_uv(uv))
}

/// The transition wrapper: `from` at progress 0 and below, `to` at 1 and
/// above, the transition's own shading between.
#[inline]
pub(crate) fn transition_row(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
    shade: impl Fn(u32, [f32; 2]) -> [f32; 4],
) {
    if context.progress() <= 0.0 {
        return shade_row(context, y, x0, out, |_, uv| context.from_color(uv));
    }
    if context.progress() >= 1.0 {
        return shade_row(context, y, x0, out, |_, uv| context.to_color(uv));
    }
    shade_row(context, y, x0, out, shade);
}

/// The PCG hash the built-in shaders use (`pcg` in their bodies): one
/// round of the PCG-RXS-M-XS 32-bit output function.
#[inline]
pub(crate) const fn pcg(value: u32) -> u32 {
    let state = value.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// `builtin_hash`: [`pcg`].
#[inline]
pub(crate) const fn builtin_hash(value: u32) -> u32 {
    pcg(value)
}

/// `builtin_hash3`.
#[inline]
pub(crate) const fn builtin_hash3(a: u32, b: u32, c: u32) -> u32 {
    pcg(a ^ pcg(b ^ pcg(c)))
}

/// `builtin_unit`: the top 24 bits of a hash as a float in `[0, 1)`,
/// converted exactly.
#[inline]
#[allow(clippy::cast_precision_loss)]
pub(crate) fn builtin_unit(hash: u32) -> f32 {
    (hash >> 8) as f32 / 16_777_216.0
}

/// `builtin_cell`: a lattice cell index of either sign as a hash input.
/// WGSL's `i32(..)` clamps to the f32 values an `i32` holds, so past 2^31
/// it is 2147483520, not `i32::MAX`.
#[inline]
#[allow(clippy::cast_possible_truncation)]
pub(crate) fn builtin_cell(value: f32) -> u32 {
    (value.floor().clamp(-2_147_483_648.0, 2_147_483_520.0) as i32).cast_unsigned()
}

/// `builtin_floor_mod`: GLSL's `mod`.
#[inline]
pub(crate) fn builtin_floor_mod(x: f32, y: f32) -> f32 {
    x - y * (x / y).floor()
}

/// `builtin_value_noise`: value noise in `[0, 1]` on the integer lattice,
/// smooth between lattice points.
#[inline]
#[allow(clippy::many_single_char_names)]
pub(crate) fn builtin_value_noise(p: [f32; 2], seed: u32) -> f32 {
    let cell = [p[0].floor(), p[1].floor()];
    let f = [p[0] - cell[0], p[1] - cell[1]];
    let w = f.map(|f| f * f * (3.0 - 2.0 * f));
    let x = builtin_cell(cell[0]);
    let y = builtin_cell(cell[1]);
    let a = builtin_unit(builtin_hash3(x, y, seed));
    let b = builtin_unit(builtin_hash3(x.wrapping_add(1), y, seed));
    let c = builtin_unit(builtin_hash3(x, y.wrapping_add(1), seed));
    let d = builtin_unit(builtin_hash3(x.wrapping_add(1), y.wrapping_add(1), seed));
    mix(mix(a, b, w[0]), mix(c, d, w[0]), w[1])
}

/// `builtin_over(top, bottom)`, both premultiplied.
#[inline]
pub(crate) fn builtin_over(top: [f32; 4], bottom: [f32; 4]) -> [f32; 4] {
    let rest = 1.0 - top[3];
    [
        top[0] + bottom[0] * rest,
        top[1] + bottom[1] * rest,
        top[2] + bottom[2] * rest,
        top[3] + bottom[3] * rest,
    ]
}

/// `builtin_valid`: clamped into premultiplied range, NaN to 0 as GPUs
/// clamp.
#[inline]
#[allow(clippy::manual_clamp)]
pub(crate) fn builtin_valid(color: [f32; 4]) -> [f32; 4] {
    let alpha = color[3].max(0.0).min(1.0);
    [
        color[0].max(0.0).min(alpha),
        color[1].max(0.0).min(alpha),
        color[2].max(0.0).min(alpha),
        alpha,
    ]
}

/// `builtin_rgb_to_hsv`: hue, saturation and value (Smith 1978), hue in
/// `[0, 1)`.
#[inline]
#[allow(clippy::float_cmp)]
pub(crate) fn builtin_rgb_to_hsv(c: [f32; 3]) -> [f32; 3] {
    let high = c[0].max(c[1].max(c[2]));
    let low = c[0].min(c[1].min(c[2]));
    let range = high - low;
    let mut hue = 0.0;
    if range > 0.0 {
        if high == c[0] {
            hue = (c[1] - c[2]) / range;
            if hue < 0.0 {
                hue += 6.0;
            }
        } else if high == c[1] {
            hue = (c[2] - c[0]) / range + 2.0;
        } else {
            hue = (c[0] - c[1]) / range + 4.0;
        }
    }
    let saturation = if high > 0.0 { range / high } else { 0.0 };
    [hue / 6.0, saturation, high]
}

/// `builtin_hsv_to_rgb`. WGSL `%` truncates; its operands here are
/// positive, where Rust's `%` is the same.
#[inline]
pub(crate) fn builtin_hsv_to_rgb(hsv: [f32; 3]) -> [f32; 3] {
    let h = fract(hsv[0]) * 6.0;
    [5.0_f32, 3.0, 1.0].map(|n| {
        let k = (n + h) % 6.0;
        let ramp = k.min(4.0 - k).clamp(0.0, 1.0);
        hsv[2] - hsv[2] * hsv[1] * ramp
    })
}
