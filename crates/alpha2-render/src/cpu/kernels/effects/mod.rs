//! The public built-in effects (builtins `effects`), one module per catalog
//! group as the definitions are, ported from their bodies in
//! `crates/alpha2-shaders/shaders/runtime/effects`. This module holds the
//! ports of the shared effect helpers (`helpers.wgsl`).

use super::{CpuKernel, KernelContext, fwidth_fine, pixel_uv};
use crate::RenderSize;
use crate::builtins::{
    DIRECTIONAL_BLUR_ID, DROP_SHADOW_ID, GAUSSIAN_BLUR_ID, GLOW_ID, TILT_SHIFT_ID, UNSHARP_MASK_ID,
};
use crate::cpu::curves;
use crate::cpu::image::{CpuImage, PixelBox};

mod blur;
mod color;
mod distort;
mod keying;
mod stylize;

/// The kernel for the built-in `id` (version 1), if this module has it.
pub(super) fn kernel(id: &str) -> Option<&'static dyn CpuKernel> {
    match id {
        GAUSSIAN_BLUR_ID | GLOW_ID | DIRECTIONAL_BLUR_ID | UNSHARP_MASK_ID | TILT_SHIFT_ID
        | DROP_SHADOW_ID => Some(&Expanded),
        _ => color::kernel(id)
            .or_else(|| keying::kernel(id))
            .or_else(|| blur::kernel(id))
            .or_else(|| stylize::kernel(id))
            .or_else(|| distort::kernel(id)),
    }
}

/// `sample_src(uv)` at the centre of pixel `(x, y)` of `image`: the texel
/// itself when the picture is the output's size, as a GPU sampler reads it
/// there (its fixed-point coordinates land on the centre exactly).
#[inline]
pub(super) fn centre(image: &CpuImage, size: RenderSize, x: u32, y: u32) -> [f32; 4] {
    if image.size() == size {
        image.texel(x, y)
    } else {
        let uv = pixel_uv(size, x, y);
        image.sample(uv[0], uv[1])
    }
}

/// Fills `out` with `effect(x, uv, src)` for the pixels `x0..` of row `y`,
/// `src` the source at the pixel, as the effect wrapper passes it.
#[inline]
pub(super) fn effect_row(
    context: &KernelContext<'_>,
    y: u32,
    x0: u32,
    out: &mut [[f32; 4]],
    effect: impl Fn(u32, [f32; 2], [f32; 4]) -> [f32; 4],
) {
    for (offset, pixel) in out.iter_mut().enumerate() {
        #[allow(clippy::cast_possible_truncation)]
        let x = x0 + offset as u32;
        let src = centre(context.source_a, context.size, x, y);
        *pixel = effect(x, pixel_uv(context.size, x, y), src);
    }
}

/// `box` grown by `x` columns and `y` rows on each side, kept inside `size`.
pub(super) fn grow(region: PixelBox, x: u32, y: u32, size: RenderSize) -> PixelBox {
    if region.is_empty() {
        return region;
    }
    let left = region.x.saturating_sub(x);
    let top = region.y.saturating_sub(y);
    let right = region.right().saturating_add(x).min(size.width);
    let bottom = region.bottom().saturating_add(y).min(size.height);
    PixelBox {
        x: left,
        y: top,
        width: right.saturating_sub(left),
        height: bottom.saturating_sub(top),
    }
}

/// A margin of `pixels` (non-negative) rounded up, one more for the
/// bilinear neighbour and one for rounding.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub(super) fn margin(pixels: f32) -> u32 {
    if pixels.is_finite() {
        (pixels.abs().ceil().min(1.0e9) as u32).saturating_add(2)
    } else {
        u32::MAX
    }
}

/// WGSL `clamp(value, 0.0, 1.0)` as GPUs evaluate it, `min(max(..))`: NaN
/// becomes 0.
#[inline]
#[allow(clippy::manual_clamp)]
pub(super) fn saturate(value: f32) -> f32 {
    value.max(0.0).min(1.0)
}

/// `dot(rgb, vec3f(0.2126, 0.7152, 0.0722))`.
#[inline]
pub(super) fn luminance(rgb: [f32; 3]) -> f32 {
    rgb[0] * 0.2126 + rgb[1] * 0.7152 + rgb[2] * 0.0722
}

/// The straight color of `color`, black below `floor` of alpha (each body
/// has its own floor).
#[inline]
pub(super) fn straight(color: [f32; 4], floor: f32) -> [f32; 3] {
    if color[3] <= floor {
        return [0.0; 3];
    }
    [
        color[0] / color[3],
        color[1] / color[3],
        color[2] / color[3],
    ]
}

/// `rgb * alpha, alpha`.
#[inline]
pub(super) fn premultiplied(rgb: [f32; 3], alpha: f32) -> [f32; 4] {
    [rgb[0] * alpha, rgb[1] * alpha, rgb[2] * alpha, alpha]
}

/// The sRGB EOTF (`builtin_decode`), through the shared table on `0..1`;
/// a straight color past one (half-float storage rounds alpha down) takes
/// the curve itself.
#[inline]
pub(super) fn decode(encoded: f32) -> f32 {
    if encoded <= 0.040_45 {
        return encoded / 12.92;
    }
    if encoded < 1.0 {
        return curves::srgb_to_linear(encoded);
    }
    libm::powf((encoded + 0.055) / 1.055, 2.4)
}

/// `builtin_encode`, the sRGB inverse EOTF, through the shared table on
/// `0..1`; past one it takes the curve itself.
#[inline]
pub(super) fn encode(linear: f32) -> f32 {
    if linear < 1.0 {
        return curves::linear_to_srgb(linear);
    }
    1.055 * libm::powf(linear, 1.0 / 2.4) - 0.055
}

/// `builtin_decode3`.
#[inline]
pub(super) fn decode3(rgb: [f32; 3]) -> [f32; 3] {
    rgb.map(decode)
}

/// `builtin_output(rgb, alpha)`: a straight color and an alpha as the
/// premultiplied value an effect returns, both clamped to `0..1`.
#[inline]
pub(super) fn output(rgb: [f32; 3], alpha: f32) -> [f32; 4] {
    let alpha = saturate(alpha);
    premultiplied(rgb.map(saturate), alpha)
}

/// `builtin_gray`: the BT.709 luminance of an encoded color in linear
/// light, encoded again.
#[inline]
pub(super) fn gray(rgb: [f32; 3]) -> f32 {
    encode(saturate(luminance(decode3(rgb))))
}

/// `builtin_seed`: a `seed` parameter as a hash input.
#[inline]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::manual_clamp
)]
pub(super) fn seed(value: f32) -> u32 {
    // WGSL `round` takes ties to even; `clamp` turns NaN into 0.
    value.round_ties_even().max(0.0).min(16_777_215.0) as u32
}

/// `builtin_aspect()`: positions in output heights.
#[inline]
pub(super) fn aspect(context: &KernelContext<'_>) -> [f32; 2] {
    let resolution = context.resolution();
    [resolution[0] / resolution[1], 1.0]
}

/// `builtin_rotate(v, radians)`.
#[inline]
pub(super) fn rotate(v: [f32; 2], radians: f32) -> [f32; 2] {
    let c = libm::cosf(radians);
    let s = libm::sinf(radians);
    [c * v[0] - s * v[1], s * v[0] + c * v[1]]
}

/// `builtin_wrap(uv, mode)`: the edge modes 0 transparent, 1 clamp, 2
/// repeat, 3 mirror.
#[inline]
pub(super) fn wrap(uv: [f32; 2], mode: u32) -> [f32; 2] {
    match mode {
        2 => uv.map(super::fract),
        3 => uv.map(|value| 1.0 - (1.0 - 2.0 * super::fract(value * 0.5)).abs()),
        _ => uv,
    }
}

/// `builtin_edge_coverage(uv, width, mode)`, `width` the `fwidthFine` of
/// `uv` ([`uv_width`]).
#[inline]
pub(super) fn edge_coverage(uv: [f32; 2], width: [f32; 2], mode: u32) -> f32 {
    if mode != 0 {
        return 1.0;
    }
    super::builtin_interval_coverage(uv[0], width[0])
        * super::builtin_interval_coverage(uv[1], width[1])
}

/// `fwidthFine(source)` at pixel `(x, y)` for a source position `source`
/// that is a function of the shader's `uv`.
#[inline]
pub(super) fn uv_width(
    size: RenderSize,
    x: u32,
    y: u32,
    source: impl Fn([f32; 2]) -> [f32; 2],
) -> [f32; 2] {
    [
        fwidth_fine(size, x, y, |uv| source(uv)[0]),
        fwidth_fine(size, x, y, |uv| source(uv)[1]),
    ]
}

/// `builtin_sample_edge(uv, width, mode)`.
#[inline]
pub(super) fn sample_edge(
    context: &KernelContext<'_>,
    uv: [f32; 2],
    width: [f32; 2],
    mode: u32,
) -> [f32; 4] {
    let coverage = edge_coverage(uv, width, mode);
    context
        .sample_src(wrap(uv, mode))
        .map(|value| value * coverage)
}

/// The effects drawn as internal passes (blurs, glow, drop shadow): the graph never draws
/// their own body, which returns the source. The kernel is here so the CPU
/// catalog lists them as project effects.
struct Expanded;

impl CpuKernel for Expanded {
    fn bounds(&self, context: &KernelContext<'_>) -> PixelBox {
        context.source_a.bounds()
    }

    fn row(&self, context: &KernelContext<'_>, y: u32, x0: u32, out: &mut [[f32; 4]]) {
        effect_row(context, y, x0, out, |_, _, src| src);
    }
}

/// Entries per octave of the `pow` tables, and the fraction bits of an f32
/// mantissa below one entry.
const POW_STEPS: usize = 1024;
const POW_SHIFT: u32 = 13;

/// `log2` over one octave's mantissa and `exp2` over `[0, 1]`, built once
/// with `libm` and read with linear interpolation: a `pow` that is the same
/// on every target and needs no library call per pixel. Its error, a few
/// millionths relative, is far under a half float's step.
pub(super) struct PowTables {
    log2: Vec<f32>,
    exp2: Vec<f32>,
}

#[allow(clippy::cast_precision_loss)]
pub(super) fn pow_tables() -> &'static PowTables {
    static TABLES: std::sync::OnceLock<PowTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| PowTables {
        log2: (0..=POW_STEPS)
            .map(|step| libm::log2f(1.0 + step as f32 / POW_STEPS as f32))
            .collect(),
        exp2: (0..=POW_STEPS)
            .map(|step| libm::exp2f(step as f32 / POW_STEPS as f32))
            .collect(),
    })
}

/// `pow(base, exponent)` for a normal, positive `base`, as
/// `exp2(exponent * log2(base))`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub(super) fn table_pow(tables: &PowTables, base: f32, exponent: f32) -> f32 {
    let bits = base.to_bits();
    let octave = ((bits >> 23) & 0xff) as i32 - 127;
    let mantissa = bits & 0x007f_ffff;
    let index = (mantissa >> POW_SHIFT) as usize;
    let fraction = (mantissa & ((1 << POW_SHIFT) - 1)) as f32 * (1.0 / (1 << POW_SHIFT) as f32);
    let low = tables.log2[index];
    let log2 = octave as f32 + (low + (tables.log2[index + 1] - low) * fraction);
    let power = exponent * log2;
    if power.is_nan() || power <= -126.0 {
        return 0.0;
    }
    if power >= 128.0 {
        return f32::INFINITY;
    }
    let whole = power.floor();
    let position = (power - whole) * POW_STEPS as f32;
    let index = (position as usize).min(POW_STEPS - 1);
    let fraction = position - index as f32;
    let low = tables.exp2[index];
    let scale = f32::from_bits(((whole as i32 + 127) as u32) << 23);
    (low + (tables.exp2[index + 1] - low) * fraction) * scale
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // The reference is the platform's `f64` math.
mod tests {
    use super::{pow_tables, table_pow, wrap};
    use crate::cpu::kernels::{builtin_hsv_to_rgb, builtin_rgb_to_hsv, builtin_value_noise};

    #[test]
    fn hsv_and_back_is_the_color_again() {
        for r in 0..=8_u8 {
            for g in 0..=8_u8 {
                for b in 0..=8_u8 {
                    let rgb = [r, g, b].map(|value| f32::from(value) / 8.0);
                    let back = builtin_hsv_to_rgb(builtin_rgb_to_hsv(rgb));
                    for (channel, value) in back.iter().zip(rgb) {
                        assert!((channel - value).abs() < 1.0e-5, "{rgb:?} -> {back:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn value_noise_stays_in_zero_to_one_on_both_sides_of_the_origin() {
        let (mut low, mut high) = (1.0_f32, 0.0_f32);
        for step in -400..400_i16 {
            let p = [f32::from(step) * 0.173, f32::from(step) * -0.311 + 0.5];
            let value = builtin_value_noise(p, 7);
            low = low.min(value);
            high = high.max(value);
        }
        assert!(low >= 0.0 && high <= 1.0, "{low}..{high}");
        assert!(high - low > 0.5, "the noise varies: {low}..{high}");
    }

    #[test]
    fn the_edge_modes_wrap_as_named() {
        let near =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1.0e-6 && (a[1] - b[1]).abs() < 1.0e-6;
        let outside = [-0.25, 1.25];
        assert_eq!(wrap(outside, 0), outside);
        assert_eq!(wrap(outside, 1), outside);
        assert!(
            near(wrap(outside, 2), [0.75, 0.25]),
            "{:?}",
            wrap(outside, 2)
        );
        assert!(
            near(wrap(outside, 3), [0.25, 0.75]),
            "{:?}",
            wrap(outside, 3)
        );
        assert!(
            near(wrap([0.4, 0.6], 3), [0.4, 0.6]),
            "{:?}",
            wrap([0.4, 0.6], 3)
        );
    }

    // Far under a half float's step (about 5e-4 relative); the bound is
    // the f32 rounding of `exponent * log2(base)` at the smallest bases.
    #[test]
    fn the_table_pow_stays_far_below_a_half_float_step() {
        let tables = pow_tables();
        let mut worst = 0.0_f64;
        for exponent in [0.0_f32, 0.05, 0.3, 0.77, 1.0, 1.5, 2.0] {
            let mut base = 1.0e-10_f32;
            while base < 64.0 {
                let expected = f64::from(base).powf(f64::from(exponent));
                let got = f64::from(table_pow(tables, base, exponent));
                worst = worst.max((got - expected).abs() / expected);
                base *= 1.000_37;
            }
        }
        assert!(worst < 1.0e-5, "relative error {worst}");
    }
}
