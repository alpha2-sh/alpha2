//! WGSL built-ins and the shared WGSL helpers kernels port: the
//! transitions' `helpers.wgsl`, and fine derivatives emulated on 2 x 2
//! quads exactly as WGSL defines them.

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
