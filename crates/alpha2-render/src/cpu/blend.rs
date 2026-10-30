//! Compositing one premultiplied pixel over another: composite.wesl's
//! `composite`, W3C Compositing and Blending Level 1 on straight colors.

use super::curves::{linear_to_srgb, srgb_to_linear};
use crate::BlendMode;

/// White as the dodge and burn branches test for it: within 1/512 of 1, as
/// composite.wesl's `BLEND_WHITE`.
const BLEND_WHITE: f32 = 1.0 - 1.0 / 512.0;

type Rgb = [f32; 3];

/// The straight color of `encoded`, in linear light when `linear_light`.
#[inline]
fn straight(encoded: [f32; 4], linear_light: bool) -> Rgb {
    if encoded[3] <= 0.000_001 {
        return [0.0; 3];
    }
    let alpha = encoded[3];
    let color = [
        (encoded[0] / alpha).clamp(0.0, 1.0),
        (encoded[1] / alpha).clamp(0.0, 1.0),
        (encoded[2] / alpha).clamp(0.0, 1.0),
    ];
    if linear_light {
        color.map(srgb_to_linear)
    } else {
        color
    }
}

#[inline]
fn screen(backdrop: f32, source: f32) -> f32 {
    backdrop + source - backdrop * source
}

#[inline]
fn hard_light(backdrop: f32, source: f32) -> f32 {
    if source <= 0.5 {
        backdrop * (2.0 * source)
    } else {
        screen(backdrop, 2.0 * source - 1.0)
    }
}

#[inline]
fn color_dodge(backdrop: f32, source: f32) -> f32 {
    if backdrop <= 0.0 {
        0.0
    } else if source >= BLEND_WHITE {
        1.0
    } else {
        (backdrop / (1.0 - source).max(0.000_001)).min(1.0)
    }
}

#[inline]
fn color_burn(backdrop: f32, source: f32) -> f32 {
    if backdrop >= BLEND_WHITE {
        1.0
    } else if source <= 0.0 {
        0.0
    } else {
        1.0 - ((1.0 - backdrop) / source.max(0.000_001)).min(1.0)
    }
}

#[inline]
fn soft_light(backdrop: f32, source: f32) -> f32 {
    if source <= 0.5 {
        backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop)
    } else {
        let d = if backdrop <= 0.25 {
            ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop
        } else {
            backdrop.sqrt()
        };
        backdrop + (2.0 * source - 1.0) * (d - backdrop)
    }
}

#[inline]
fn lum(color: Rgb) -> f32 {
    color[0] * 0.3 + color[1] * 0.59 + color[2] * 0.11
}

fn clip_color(color: Rgb) -> Rgb {
    let l = lum(color);
    let n = color[0].min(color[1].min(color[2]));
    let x = color[0].max(color[1].max(color[2]));
    let mut clipped = color;
    if n < 0.0 && l - n > 0.0 {
        clipped = clipped.map(|c| l + (c - l) * l / (l - n));
    }
    if x > 1.0 && x - l > 0.0 {
        clipped = clipped.map(|c| l + (c - l) * (1.0 - l) / (x - l));
    }
    clipped
}

fn set_lum(color: Rgb, l: f32) -> Rgb {
    let d = l - lum(color);
    clip_color(color.map(|c| c + d))
}

fn sat(color: Rgb) -> f32 {
    color[0].max(color[1].max(color[2])) - color[0].min(color[1].min(color[2]))
}

fn set_sat(color: Rgb, s: f32) -> Rgb {
    let high = color[0].max(color[1].max(color[2]));
    let low = color[0].min(color[1].min(color[2]));
    if high > low {
        color.map(|c| (c - low) * s / (high - low))
    } else {
        [0.0; 3]
    }
}

/// B(Cb, Cs) for `mode`, composite.wesl's `blend_rgb`.
fn blend_rgb(backdrop: Rgb, source: Rgb, mode: BlendMode) -> Rgb {
    let separable = |op: fn(f32, f32) -> f32| {
        [
            op(backdrop[0], source[0]),
            op(backdrop[1], source[1]),
            op(backdrop[2], source[2]),
        ]
    };
    match mode {
        // `composite` adds instead of blending for `PlusLighter`, so it never reaches here.
        BlendMode::Normal | BlendMode::PlusLighter => source,
        BlendMode::Multiply => separable(|b, s| b * s),
        BlendMode::Screen => separable(screen),
        BlendMode::Overlay => separable(|b, s| hard_light(s, b)),
        BlendMode::Darken => separable(f32::min),
        BlendMode::Lighten => separable(f32::max),
        BlendMode::ColorDodge => separable(color_dodge),
        BlendMode::ColorBurn => separable(color_burn),
        BlendMode::HardLight => separable(hard_light),
        BlendMode::SoftLight => separable(soft_light),
        BlendMode::Difference => separable(|b, s| (b - s).abs()),
        BlendMode::Exclusion => separable(|b, s| b + s - 2.0 * b * s),
        BlendMode::Hue => set_lum(set_sat(source, sat(backdrop)), lum(backdrop)),
        BlendMode::Saturation => set_lum(set_sat(backdrop, sat(source)), lum(backdrop)),
        BlendMode::Color => set_lum(source, lum(backdrop)),
        BlendMode::Luminosity => set_lum(backdrop, lum(source)),
    }
}

/// `source` composited over `backdrop` with `mode`, both premultiplied and
/// sRGB-encoded, blended in linear light or in the encoded space.
#[inline]
pub(crate) fn composite(
    backdrop: [f32; 4],
    source: [f32; 4],
    mode: BlendMode,
    linear_light: bool,
) -> [f32; 4] {
    let source_alpha = source[3];
    let backdrop_alpha = backdrop[3];
    let s = straight(source, linear_light);
    let b = straight(backdrop, linear_light);
    if mode == BlendMode::PlusLighter {
        return plus_lighter(b, backdrop_alpha, s, source_alpha, linear_light);
    }
    let blended = blend_rgb(b, s, mode);
    let output_alpha = source_alpha + backdrop_alpha * (1.0 - source_alpha);
    if output_alpha <= 0.000_001 {
        return [0.0; 4];
    }
    let source_weight = source_alpha * (1.0 - backdrop_alpha);
    let backdrop_weight = backdrop_alpha * (1.0 - source_alpha);
    let blend_weight = source_alpha * backdrop_alpha;
    let mut out = [0.0; 4];
    for channel in 0..3 {
        let premultiplied = source_weight * s[channel]
            + backdrop_weight * b[channel]
            + blend_weight * blended[channel];
        let mut color = (premultiplied / output_alpha).clamp(0.0, 1.0);
        if linear_light {
            color = linear_to_srgb(color);
        }
        out[channel] = color * output_alpha;
    }
    out[3] = output_alpha;
    out
}

/// composite.wesl's `plus_lighter`: premultiplied color and alpha added in
/// the blend space, each clamped to 1.
fn plus_lighter(
    backdrop: Rgb,
    backdrop_alpha: f32,
    source: Rgb,
    source_alpha: f32,
    linear_light: bool,
) -> [f32; 4] {
    let output_alpha = (source_alpha + backdrop_alpha).min(1.0);
    if output_alpha <= 0.000_001 {
        return [0.0; 4];
    }
    let mut out = [0.0; 4];
    for channel in 0..3 {
        let premultiplied =
            (source[channel] * source_alpha + backdrop[channel] * backdrop_alpha).min(1.0);
        let mut color = (premultiplied / output_alpha).clamp(0.0, 1.0);
        if linear_light {
            color = linear_to_srgb(color);
        }
        out[channel] = color * output_alpha;
    }
    out[3] = output_alpha;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_opaque_normal_source_replaces_the_backdrop() {
        let out = composite(
            [0.2, 0.3, 0.4, 1.0],
            [0.5, 0.25, 0.75, 1.0],
            BlendMode::Normal,
            true,
        );
        for (got, want) in out.iter().zip([0.5, 0.25, 0.75, 1.0]) {
            assert!((got - want).abs() < 1.0e-5, "{out:?}");
        }
    }

    #[test]
    fn a_transparent_source_leaves_the_backdrop() {
        let backdrop = [0.2, 0.3, 0.4, 0.8];
        let out = composite(backdrop, [0.0; 4], BlendMode::Multiply, false);
        for (got, want) in out.iter().zip(backdrop) {
            assert!((got - want).abs() < 1.0e-6, "{out:?}");
        }
    }
}
