//! The transfer curves the passes and the source presenter convert with.
//!
//! Shaders call `pow` and `exp`, which GPUs approximate. Here each curve is a `libm`-built table
//! (identical on every target) linearly interpolated at fixed steps per octave, keeping error far
//! below a half float's step.

use std::sync::OnceLock;

use crate::hdr;

/// Fraction bits below a table step of the sRGB curves: `2^(23 - 13)` = 1024 steps per octave.
const STEP_SHIFT: u32 = 13;

/// One curve on `[2^low_octave, 1)`, sampled at every step.
struct Curve {
    /// Bits of `2^low_octave`, the first entry's input.
    base: u32,
    /// Fraction bits below a step: `2^(23 - shift)` steps per octave.
    shift: u32,
    values: Vec<f32>,
}

impl Curve {
    fn build(low_octave: i32, shift: u32, exact: impl Fn(f32) -> f32) -> Self {
        #[allow(clippy::cast_sign_loss)]
        let base = ((127 + low_octave) as u32) << 23;
        let end = 127_u32 << 23;
        let steps = (end - base) >> shift;
        let values = (0..=steps)
            .map(|step| exact(f32::from_bits(base + (step << shift))))
            .collect();
        Self {
            base,
            shift,
            values,
        }
    }

    /// The curve at `value` in `[2^low_octave, 1)`.
    #[inline]
    fn at(&self, value: f32) -> f32 {
        let offset = value.to_bits() - self.base;
        let index = (offset >> self.shift) as usize;
        #[allow(clippy::cast_precision_loss)]
        let fraction = (offset & ((1 << self.shift) - 1)) as f32 * (1.0 / (1 << self.shift) as f32);
        let low = self.values[index];
        low + (self.values[index + 1] - low) * fraction
    }

    /// Whether `value` lies in the table's range `[2^low_octave, 1)`; false for NaN.
    #[inline]
    fn covers(&self, value: f32) -> bool {
        value >= f32::from_bits(self.base) && value < 1.0
    }
}

struct Curves {
    to_linear: Curve,
    to_srgb: Curve,
}

fn curves() -> &'static Curves {
    static CURVES: OnceLock<Curves> = OnceLock::new();
    CURVES.get_or_init(|| Curves {
        // 0.04045 lies in [2^-5, 2^-4), 0.0031308 in [2^-9, 2^-8).
        to_linear: Curve::build(-5, STEP_SHIFT, exact_srgb_to_linear),
        to_srgb: Curve::build(-9, STEP_SHIFT, exact_linear_to_srgb),
    })
}

/// The presenter's HDR curves, built from the `f64` references in [`crate::hdr`]. Inputs below a
/// table's first octave are black and take the reference.
struct HdrCurves {
    /// [`hdr::pq_eotf`] over codes in `[2^-14, 1)`.
    pq_eotf: Curve,
    /// [`hdr::pq_inverse_eotf`] over `nits / 10000` in `[2^-24, 1)`.
    pq_inverse: Curve,
    /// The upper branch of [`hdr::hlg_inverse_oetf`] over `[0.5, 1)`.
    hlg: Curve,
}

/// Fraction bits below a step of the HDR curves: 4096 steps per octave.
const HDR_SHIFT: u32 = 11;

fn hdr_curves() -> &'static HdrCurves {
    static CURVES: OnceLock<HdrCurves> = OnceLock::new();
    CURVES.get_or_init(|| HdrCurves {
        pq_eotf: Curve::build(-14, HDR_SHIFT, hdr::pq_eotf),
        pq_inverse: Curve::build(-24, HDR_SHIFT, |y| hdr::pq_inverse_eotf(y * 10_000.0)),
        hlg: Curve::build(-1, HDR_SHIFT, hdr::hlg_inverse_oetf),
    })
}

/// [`hdr::pq_eotf`] from its table.
#[inline]
pub(crate) fn pq_eotf(code: f32) -> f32 {
    let code = code.clamp(0.0, 1.0);
    let table = &hdr_curves().pq_eotf;
    if table.covers(code) {
        table.at(code)
    } else {
        hdr::pq_eotf(code)
    }
}

/// [`hdr::pq_inverse_eotf`] from its table.
#[inline]
pub(crate) fn pq_inverse_eotf(nits: f32) -> f32 {
    let y = (nits / 10_000.0).clamp(0.0, 1.0);
    let table = &hdr_curves().pq_inverse;
    if table.covers(y) {
        table.at(y)
    } else {
        hdr::pq_inverse_eotf(nits)
    }
}

/// [`hdr::hlg_inverse_oetf`] from its table.
#[inline]
pub(crate) fn hlg_inverse_oetf(code: f32) -> f32 {
    let e = code.clamp(0.0, 1.0);
    let table = &hdr_curves().hlg;
    if e > 0.5 && table.covers(e) {
        table.at(e)
    } else {
        hdr::hlg_inverse_oetf(e)
    }
}

/// composite.wesl's exact sRGB EOTF, via `libm`.
pub(crate) fn exact_srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        return value / 12.92;
    }
    if value >= 1.0 {
        return 1.0;
    }
    libm::powf((value + 0.055) / 1.055, 2.4)
}

/// composite.wesl's exact sRGB inverse EOTF.
pub(crate) fn exact_linear_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        return value * 12.92;
    }
    if value >= 1.0 {
        return 1.0;
    }
    1.055 * libm::powf(value, 1.0 / 2.4) - 0.055
}

/// `srgb_to_linear_component` of composite.wesl; both ends map exactly.
#[inline]
pub(crate) fn srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        return value / 12.92;
    }
    if value >= 1.0 {
        return 1.0;
    }
    curves().to_linear.at(value)
}

/// `linear_to_srgb_component` of composite.wesl.
#[inline]
pub(crate) fn linear_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        return value * 12.92;
    }
    if value >= 1.0 {
        return 1.0;
    }
    curves().to_srgb.at(value)
}

/// sRGB curve odd-extended below zero and continued above one (`extended_srgb` of
/// rgba-present.wesl), for linear sources outside `0..1`.
pub(crate) fn extended_linear_to_srgb(value: f32) -> f32 {
    let magnitude = value.abs();
    let encoded = if magnitude <= 0.003_130_8 {
        12.92 * magnitude
    } else if magnitude < 1.0 {
        curves().to_srgb.at(magnitude)
    } else {
        1.055 * libm::powf(magnitude, 1.0 / 2.4) - 0.055
    };
    if value < 0.0 { -encoded } else { encoded }
}

/// Entries per octave of the `pow` tables.
const POW_STEPS: usize = 1024;
const POW_SHIFT: u32 = 13;

/// `log2` over one octave's mantissa and `exp2` over `[0, 1]`, built once with `libm` and
/// linearly interpolated: a target-independent `pow` with no per-pixel library call, accurate
/// to a few millionths relative.
pub(crate) struct PowTables {
    log2: Vec<f32>,
    exp2: Vec<f32>,
}

#[allow(clippy::cast_precision_loss)]
pub(crate) fn pow_tables() -> &'static PowTables {
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

/// `pow(base, exponent)` for normal positive `base`, as `exp2(exponent * log2(base))`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap
)]
pub(crate) fn table_pow(tables: &PowTables, base: f32, exponent: f32) -> f32 {
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
mod tests {
    use super::*;

    /// Largest table error against the exact curve, in half-float steps at the curve's value.
    fn worst_error(table: impl Fn(f32) -> f32, exact: impl Fn(f32) -> f32, from: f32) -> f32 {
        let mut worst = 0.0_f32;
        let mut bits = from.to_bits();
        while f32::from_bits(bits) < 1.0 {
            let value = f32::from_bits(bits);
            let expected = exact(value);
            let step = libm::exp2f(libm::floorf(libm::log2f(expected.max(1.0e-4)))) / 1024.0;
            worst = worst.max((table(value) - expected).abs() / step);
            bits += 97;
        }
        worst
    }

    #[test]
    fn the_tables_stay_far_below_a_half_float_step() {
        assert!(worst_error(srgb_to_linear, exact_srgb_to_linear, 0.040_45) < 0.01);
        assert!(worst_error(linear_to_srgb, exact_linear_to_srgb, 0.003_130_8) < 0.01);
    }

    #[test]
    fn both_curves_map_their_ends_exactly() {
        assert_eq!(srgb_to_linear(0.0), 0.0);
        assert_eq!(srgb_to_linear(1.0), 1.0);
        assert_eq!(linear_to_srgb(0.0), 0.0);
        assert_eq!(linear_to_srgb(1.0), 1.0);
        let below_one = f32::from_bits(1.0_f32.to_bits() - 1);
        assert!(linear_to_srgb(below_one) <= 1.0);
        assert!(srgb_to_linear(below_one) <= 1.0);
        assert_eq!(extended_linear_to_srgb(-0.5), -extended_linear_to_srgb(0.5));
    }

    // Far under a half float's step (about 5e-4 relative); the bound is the f32 rounding of
    // `exponent * log2(base)` at the smallest bases.
    #[test]
    #[allow(clippy::disallowed_methods)] // The reference is the platform's `f64` math.
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
