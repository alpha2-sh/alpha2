//! The sRGB curves the passes convert with.
//!
//! The shaders call `pow`, which GPUs approximate. Here each curve is a table
//! built once with `libm` (the same bits on every target) and read with
//! linear interpolation between entries 1/1024 of an octave apart, which
//! keeps the error orders of magnitude below a half float's step.

use std::sync::OnceLock;

/// Fraction bits of an `f32` below a table step: each octave holds
/// `2^(23 - STEP_SHIFT)` = 1024 steps.
const STEP_SHIFT: u32 = 13;

/// One curve on `[2^low_octave, 1)`, sampled at every step.
struct Curve {
    /// The bits of `2^low_octave`, the first entry's input.
    base: u32,
    values: Vec<f32>,
}

impl Curve {
    fn build(low_octave: i32, exact: impl Fn(f32) -> f32) -> Self {
        #[allow(clippy::cast_sign_loss)]
        let base = ((127 + low_octave) as u32) << 23;
        let end = 127_u32 << 23;
        let steps = (end - base) >> STEP_SHIFT;
        let values = (0..=steps)
            .map(|step| exact(f32::from_bits(base + (step << STEP_SHIFT))))
            .collect();
        Self { base, values }
    }

    /// The curve at `value`, which must lie in `[2^low_octave, 1)`.
    #[inline]
    fn at(&self, value: f32) -> f32 {
        let offset = value.to_bits() - self.base;
        let index = (offset >> STEP_SHIFT) as usize;
        #[allow(clippy::cast_precision_loss)]
        let fraction = (offset & ((1 << STEP_SHIFT) - 1)) as f32 * (1.0 / (1 << STEP_SHIFT) as f32);
        let low = self.values[index];
        low + (self.values[index + 1] - low) * fraction
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
        to_linear: Curve::build(-5, exact_srgb_to_linear),
        to_srgb: Curve::build(-9, exact_linear_to_srgb),
    })
}

/// The sRGB EOTF as the composite shader defines it, exactly, with `libm`.
pub(crate) fn exact_srgb_to_linear(value: f32) -> f32 {
    if value <= 0.040_45 {
        return value / 12.92;
    }
    if value >= 1.0 {
        return 1.0;
    }
    libm::powf((value + 0.055) / 1.055, 2.4)
}

/// The sRGB inverse EOTF as the composite shader defines it, exactly.
pub(crate) fn exact_linear_to_srgb(value: f32) -> f32 {
    if value <= 0.003_130_8 {
        return value * 12.92;
    }
    if value >= 1.0 {
        return 1.0;
    }
    1.055 * libm::powf(value, 1.0 / 2.4) - 0.055
}

/// `srgb_to_linear_component` of composite.wesl: both ends map exactly, 1
/// to 1.
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

/// The sRGB curve odd-extended below zero and continued above one
/// (`extended_srgb` of rgba-present.wesl), for linear-light sources whose
/// values leave `0..1`.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The largest error of a curve's table against the curve, in units of
    /// the half float step at the curve's value.
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
}
