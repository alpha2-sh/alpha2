//! CPU reference for the W3C Compositing and Blending Level 1 blend modes,
//! and the Level 2 `plus-lighter` operator.
//!
//! The formulas follow the specification's pseudocode section by section
//! (<https://www.w3.org/TR/compositing-1/#blending>), in `f64`. Tests check
//! them against values computed independently, then hold the GPU compositor
//! to them for every mode.
// The specification's singular branches compare with 0 and 1 exactly.
#![allow(clippy::float_cmp)]

use crate::BlendMode;

/// `B(Cb, Cs)` for straight colors in `0..=1`. `PlusLighter` is not a blend
/// function; for it this is the result of two opaque inputs, their clamped sum.
pub(crate) fn blend(mode: BlendMode, backdrop: [f64; 3], source: [f64; 3]) -> [f64; 3] {
    let separable = |function: fn(f64, f64) -> f64| {
        [0, 1, 2].map(|channel| function(backdrop[channel], source[channel]))
    };
    match mode {
        BlendMode::Normal => source,
        BlendMode::Multiply => separable(multiply),
        BlendMode::Screen => separable(screen),
        BlendMode::Overlay => separable(|cb, cs| hard_light(cs, cb)),
        BlendMode::Darken => separable(f64::min),
        BlendMode::Lighten => separable(f64::max),
        BlendMode::ColorDodge => separable(color_dodge),
        BlendMode::ColorBurn => separable(color_burn),
        BlendMode::HardLight => separable(hard_light),
        BlendMode::SoftLight => separable(soft_light),
        BlendMode::Difference => separable(|cb, cs| (cb - cs).abs()),
        BlendMode::Exclusion => separable(|cb, cs| cb + cs - 2.0 * cb * cs),
        BlendMode::Hue => set_lum(set_sat(source, sat(backdrop)), lum(backdrop)),
        BlendMode::Saturation => set_lum(set_sat(backdrop, sat(source)), lum(backdrop)),
        BlendMode::Color => set_lum(source, lum(backdrop)),
        BlendMode::Luminosity => set_lum(backdrop, lum(source)),
        BlendMode::PlusLighter => separable(|cb, cs| (cb + cs).min(1.0)),
    }
}

/// The source-over result of an opaque source over an opaque backdrop, both
/// sRGB-encoded, as the compositor stores it: blended in `space`.
pub(crate) fn composite_opaque(
    mode: BlendMode,
    space: crate::BlendSpace,
    backdrop: [f64; 3],
    source: [f64; 3],
) -> [f64; 3] {
    match space {
        crate::BlendSpace::SrgbEncoded => blend(mode, backdrop, source).map(|c| c.clamp(0.0, 1.0)),
        crate::BlendSpace::LinearLight => blend(
            mode,
            backdrop.map(srgb_to_linear),
            source.map(srgb_to_linear),
        )
        .map(|c| linear_to_srgb(c.clamp(0.0, 1.0))),
    }
}

/// Source-over with blending, the specification's general formula
/// (<https://www.w3.org/TR/compositing-1/#blending>, section 5.8):
///
/// `co = as * (1 - ab) * Cs + as * ab * B(Cb, Cs) + (1 - as) * ab * Cb`,
/// `ao = as + ab * (1 - as)`,
///
/// for a `source` over a `backdrop` given as the compositor stores them,
/// premultiplied and sRGB-encoded, blended in `space`, and returned the same
/// way. Unlike [`composite_opaque`] it holds at every backdrop alpha, where
/// the `as * (1 - ab) * Cs` term is not zero.
pub(crate) fn composite(
    mode: BlendMode,
    space: crate::BlendSpace,
    backdrop: [f64; 4],
    source: [f64; 4],
) -> [f64; 4] {
    let linear = space == crate::BlendSpace::LinearLight;
    let straight = |color: [f64; 4]| -> [f64; 3] {
        if color[3] <= 0.0 {
            return [0.0; 3];
        }
        let straight = [0, 1, 2].map(|channel| (color[channel] / color[3]).clamp(0.0, 1.0));
        if linear {
            straight.map(srgb_to_linear)
        } else {
            straight
        }
    };
    let (source_alpha, backdrop_alpha) = (source[3], backdrop[3]);
    let (cs, cb) = (straight(source), straight(backdrop));
    if mode == BlendMode::PlusLighter {
        return plus_lighter(linear, backdrop_alpha, cb, source_alpha, cs);
    }
    let blended = blend(mode, cb, cs);
    let output_alpha = source_alpha + backdrop_alpha * (1.0 - source_alpha);
    if output_alpha <= 0.0 {
        return [0.0; 4];
    }
    let color = [0, 1, 2].map(|channel| {
        let premultiplied = source_alpha * (1.0 - backdrop_alpha) * cs[channel]
            + source_alpha * backdrop_alpha * blended[channel]
            + (1.0 - source_alpha) * backdrop_alpha * cb[channel];
        let color = (premultiplied / output_alpha).clamp(0.0, 1.0);
        if linear { linear_to_srgb(color) } else { color }
    });
    [
        color[0] * output_alpha,
        color[1] * output_alpha,
        color[2] * output_alpha,
        output_alpha,
    ]
}

/// Compositing Level 2 `plus-lighter`
/// (<https://www.w3.org/TR/compositing-2/#porterduffcompositingoperators_plus_lighter>):
/// `co = min(1, as * Cs + ab * Cb)`, `ao = min(1, as + ab)`, with the colors
/// straight in the blend space.
fn plus_lighter(linear: bool, ab: f64, cb: [f64; 3], source_alpha: f64, cs: [f64; 3]) -> [f64; 4] {
    let output_alpha = (source_alpha + ab).min(1.0);
    if output_alpha <= 0.0 {
        return [0.0; 4];
    }
    let color = [0, 1, 2].map(|channel| {
        let premultiplied = (source_alpha * cs[channel] + ab * cb[channel]).min(1.0);
        let color = (premultiplied / output_alpha).clamp(0.0, 1.0);
        if linear { linear_to_srgb(color) } else { color }
    });
    [
        color[0] * output_alpha,
        color[1] * output_alpha,
        color[2] * output_alpha,
        output_alpha,
    ]
}

fn multiply(cb: f64, cs: f64) -> f64 {
    cb * cs
}

fn screen(cb: f64, cs: f64) -> f64 {
    cb + cs - cb * cs
}

fn hard_light(cb: f64, cs: f64) -> f64 {
    if cs <= 0.5 {
        multiply(cb, 2.0 * cs)
    } else {
        screen(cb, 2.0 * cs - 1.0)
    }
}

fn color_dodge(cb: f64, cs: f64) -> f64 {
    if cb == 0.0 {
        0.0
    } else if cs == 1.0 {
        1.0
    } else {
        (cb / (1.0 - cs)).min(1.0)
    }
}

fn color_burn(cb: f64, cs: f64) -> f64 {
    if cb == 1.0 {
        1.0
    } else if cs == 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - cb) / cs).min(1.0)
    }
}

fn soft_light(cb: f64, cs: f64) -> f64 {
    if cs <= 0.5 {
        cb - (1.0 - 2.0 * cs) * cb * (1.0 - cb)
    } else {
        let d = if cb <= 0.25 {
            ((16.0 * cb - 12.0) * cb + 4.0) * cb
        } else {
            cb.sqrt()
        };
        cb + (2.0 * cs - 1.0) * (d - cb)
    }
}

fn lum(color: [f64; 3]) -> f64 {
    0.3 * color[0] + 0.59 * color[1] + 0.11 * color[2]
}

fn clip_color(color: [f64; 3]) -> [f64; 3] {
    let l = lum(color);
    let n = color[0].min(color[1]).min(color[2]);
    let x = color[0].max(color[1]).max(color[2]);
    let mut clipped = color;
    // A gray has no spread to scale; one a rounding error out of range would divide
    // 0 by 0.
    if n < 0.0 && l - n > 0.0 {
        clipped = clipped.map(|c| l + (c - l) * l / (l - n));
    }
    if x > 1.0 && x - l > 0.0 {
        clipped = clipped.map(|c| l + (c - l) * (1.0 - l) / (x - l));
    }
    clipped
}

fn set_lum(color: [f64; 3], l: f64) -> [f64; 3] {
    let d = l - lum(color);
    clip_color(color.map(|c| c + d))
}

fn sat(color: [f64; 3]) -> f64 {
    color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2])
}

/// The specification's `SetSat`: the largest component becomes `s`, the
/// smallest zero, and the middle one keeps its relative position.
fn set_sat(color: [f64; 3], s: f64) -> [f64; 3] {
    let max = color[0].max(color[1]).max(color[2]);
    let min = color[0].min(color[1]).min(color[2]);
    if max > min {
        color.map(|c| (c - min) * s / (max - min))
    } else {
        [0.0; 3]
    }
}

fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

fn linear_to_srgb(value: f64) -> f64 {
    if value <= 0.003_130_8 {
        value * 12.92
    } else {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    }
}

#[cfg(test)]
mod tests {
    use super::{blend, composite};
    use crate::{BlendMode, BlendSpace};

    const PAIRS: [([f64; 3], [f64; 3]); 3] = [
        ([0.5, 0.25, 0.125], [0.25, 0.5, 0.75]),
        ([0.9, 0.1, 0.6], [0.3, 0.8, 0.2]),
        ([0.2, 0.7, 0.4], [0.6, 0.35, 0.95]),
    ];

    /// `B(Cb, Cs)` for each mode and each of `PAIRS`, computed independently
    /// of this module (Python, `fractions.Fraction`, from the specification's
    /// text) and rounded to 12 decimals. `PlusLighter`'s row is the clamped
    /// sum, worked by hand.
    #[allow(clippy::unreadable_literal)]
    const EXPECTED: [(BlendMode, [[f64; 3]; 3]); 17] = include!("blend_reference_expected.in");

    #[test]
    fn the_reference_matches_independently_computed_values() {
        for (mode, expected) in EXPECTED {
            for ((backdrop, source), want) in PAIRS.into_iter().zip(expected) {
                let got = blend(mode, backdrop, source);
                for channel in 0..3 {
                    assert!(
                        (got[channel] - want[channel]).abs() < 1e-9,
                        "{mode:?} {backdrop:?} {source:?}: {got:?} != {want:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn singular_inputs_take_the_specified_branch() {
        let dodge = |cb, cs| blend(BlendMode::ColorDodge, [cb; 3], [cs; 3])[0];
        let burn = |cb, cs| blend(BlendMode::ColorBurn, [cb; 3], [cs; 3])[0];
        // A black backdrop stays black even under a white source, and a white source
        // otherwise saturates.
        assert_eq!(dodge(0.0, 1.0).to_bits(), 0.0_f64.to_bits());
        assert_eq!(dodge(0.3, 1.0).to_bits(), 1.0_f64.to_bits());
        // A white backdrop stays white even under a black source, and a black source
        // otherwise crushes.
        assert_eq!(burn(1.0, 0.0).to_bits(), 1.0_f64.to_bits());
        assert_eq!(burn(0.7, 0.0).to_bits(), 0.0_f64.to_bits());

        // A gray has no saturation: Hue and Saturation of a gray source keep the backdrop's
        // luminosity and drop its chroma.
        let backdrop = [0.8, 0.2, 0.4];
        let gray = [0.5; 3];
        let luma = 0.3 * 0.8 + 0.59 * 0.2 + 0.11 * 0.4;
        for mode in [BlendMode::Hue, BlendMode::Saturation] {
            let result = blend(mode, backdrop, gray);
            for channel in result {
                assert!((channel - luma).abs() < 1e-12, "{mode:?}: {result:?}");
            }
        }
        // Color of a gray source over any backdrop is a gray of the backdrop's luminosity;
        // Luminosity of a gray backdrop is a gray too.
        let color = blend(BlendMode::Color, backdrop, gray);
        assert!(color.iter().all(|channel| (channel - luma).abs() < 1e-12));
        let luminosity = blend(BlendMode::Luminosity, gray, backdrop);
        assert!(
            luminosity
                .iter()
                .all(|channel| (channel - luma).abs() < 1e-12)
        );
    }

    #[test]
    fn a_gray_out_of_range_by_a_rounding_error_is_left_as_it_is() {
        // In binary floating point this gray's SetLum to 1 lands a rounding error above 1:
        // ClipColor's x - l is zero, and dividing by it would give NaN.
        let gray = [0.979_952_885_890_077; 3];
        let clipped = super::set_lum(gray, 1.0);
        assert!(
            clipped
                .iter()
                .all(|c| c.is_finite() && (c - 1.0).abs() < 1e-12),
            "{clipped:?}"
        );
    }

    #[test]
    fn source_over_a_translucent_backdrop_follows_the_general_formula() {
        // Worked by hand from the specification's formula: a straight (0.8, 0.4, 0.2)
        // backdrop at alpha 0.5, a gray 0.5 source, multiply, sRGB-encoded.
        let backdrop = [0.4, 0.2, 0.1, 0.5];
        let close = |got: [f64; 4], want: [f64; 4]| {
            assert!(
                got.iter()
                    .zip(want)
                    .all(|(got, want)| (got - want).abs() < 1e-12),
                "{got:?} != {want:?}"
            );
        };
        // Opaque source: co = 0.5 * 0.5 * Cs + 0.5 * B = (0.45, 0.35, 0.30), ao = 1.
        close(
            composite(
                BlendMode::Multiply,
                BlendSpace::SrgbEncoded,
                backdrop,
                [0.5, 0.5, 0.5, 1.0],
            ),
            [0.45, 0.35, 0.30, 1.0],
        );
        // Half-transparent source: co = 0.25 * Cs + 0.25 * Cb + 0.25 * B = (0.425, 0.275, 0.2),
        // ao = 0.75.
        close(
            composite(
                BlendMode::Multiply,
                BlendSpace::SrgbEncoded,
                backdrop,
                [0.25, 0.25, 0.25, 0.5],
            ),
            [0.425, 0.275, 0.2, 0.75],
        );
        // Over a transparent backdrop the source comes through unchanged, in every mode.
        for mode in BlendMode::ALL {
            close(
                composite(
                    mode,
                    BlendSpace::SrgbEncoded,
                    [0.0; 4],
                    [0.3, 0.2, 0.1, 0.6],
                ),
                [0.3, 0.2, 0.1, 0.6],
            );
        }
    }

    #[test]
    fn plus_lighter_is_commutative_and_clamps() {
        let close = |got: [f64; 4], want: [f64; 4]| {
            assert!(
                got.iter()
                    .zip(want)
                    .all(|(got, want)| (got - want).abs() < 1e-12),
                "{got:?} != {want:?}"
            );
        };
        let a = [0.3, 0.1, 0.2, 0.6];
        let b = [0.4, 0.35, 0.05, 0.7];
        for space in [BlendSpace::SrgbEncoded, BlendSpace::LinearLight] {
            close(
                composite(BlendMode::PlusLighter, space, a, b),
                composite(BlendMode::PlusLighter, space, b, a),
            );
            // White plus white is white, and nothing added to a color leaves it.
            close(
                composite(BlendMode::PlusLighter, space, [1.0; 4], [1.0; 4]),
                [1.0; 4],
            );
            close(composite(BlendMode::PlusLighter, space, [0.0; 4], a), a);
        }
        // In the encoded space the stored premultiplied values add: alpha 0.6 + 0.7 clamps
        // to 1, red 0.3 + 0.4 stays under it.
        close(
            composite(BlendMode::PlusLighter, BlendSpace::SrgbEncoded, a, b),
            [0.7, 0.45, 0.25, 1.0],
        );
    }
}
