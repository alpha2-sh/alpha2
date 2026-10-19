use alpha2_media::{ColorMetadata, ColorPrimaries, TransferFunction};

use crate::hdr::{HdrToneMap, gamut_matrix, hdr_to_working};

/// A normalized YUV sample and straight alpha value.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct YuvSample {
    /// Luma code normalized to the storage maximum.
    pub y: f32,
    /// Blue-difference chroma code normalized to the storage maximum.
    pub u: f32,
    /// Red-difference chroma code normalized to the storage maximum.
    pub v: f32,
    /// Straight alpha.
    pub alpha: f32,
}

/// SDR transfer used when encoding a render result.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum SdrOutputTransfer {
    /// IEC sRGB.
    Srgb,
    /// BT.709 OETF.
    Bt709,
}

/// What happens to an HDR (PQ or HLG) source.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ColorPolicy {
    /// Stop rather than show it: a conversion fails with
    /// [`crate::RenderError::UnsupportedHdr`].
    RejectHdr,
    /// Return a structured warning and a transparent picture; the caller may
    /// skip the source.
    WarnHdr,
    /// Bring it into the SDR working space with this tone map
    /// ([`crate::hdr`]).
    ToneMapHdr(HdrToneMap),
}

/// Whether `color` is an HDR signal: PQ or HLG, or declared HDR.
#[must_use]
pub const fn is_hdr(color: ColorMetadata) -> bool {
    color.is_hdr()
}

/// Observable color-pipeline diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ColorDiagnostic {
    /// No metadata was supplied and BT.709 limited SDR was selected.
    MissingMetadataFallback,
    /// An HDR source was refused or skipped by the policy.
    UnsupportedHdr,
    /// An HDR source was tone-mapped into the SDR working space.
    ToneMappedHdr,
}

/// Color conversion failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ColorError {
    /// Bit depth is outside the supported 8-to-16-bit integer range.
    InvalidBitDepth,
    /// Sample contains NaN, infinity, or invalid alpha.
    InvalidSample,
    /// HDR was rejected by policy.
    UnsupportedHdr,
}

impl std::fmt::Display for ColorError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidBitDepth => "color bit depth must be within 8..=16",
            Self::InvalidSample => "color sample is non-finite or out of range",
            Self::UnsupportedHdr => "HDR input is unsupported by the selected policy",
        })
    }
}

impl std::error::Error for ColorError {}

/// Converts one integer-code-normalized YUV sample to premultiplied linear RGB.
///
/// # Errors
/// Returns an error for malformed samples, unsupported bit depth, or rejected HDR.
pub fn convert_yuv_to_linear_rgba(
    sample: YuvSample,
    bit_depth: u8,
    metadata: Option<ColorMetadata>,
    policy: ColorPolicy,
) -> Result<([f32; 4], Vec<ColorDiagnostic>), ColorError> {
    if !(8..=16).contains(&bit_depth) {
        return Err(ColorError::InvalidBitDepth);
    }
    if ![sample.y, sample.u, sample.v, sample.alpha]
        .into_iter()
        .all(f32::is_finite)
        || !(0.0..=1.0).contains(&sample.alpha)
    {
        return Err(ColorError::InvalidSample);
    }
    let mut diagnostics = Vec::new();
    let color = metadata.unwrap_or_else(|| {
        diagnostics.push(ColorDiagnostic::MissingMetadataFallback);
        ColorMetadata::UNDECLARED
    });
    let tone = if is_hdr(color) {
        match policy {
            ColorPolicy::RejectHdr => {
                diagnostics.push(ColorDiagnostic::UnsupportedHdr);
                return Err(ColorError::UnsupportedHdr);
            }
            ColorPolicy::WarnHdr => {
                diagnostics.push(ColorDiagnostic::UnsupportedHdr);
                return Ok(([0.0, 0.0, 0.0, 0.0], diagnostics));
            }
            ColorPolicy::ToneMapHdr(tone) => {
                diagnostics.push(ColorDiagnostic::ToneMappedHdr);
                Some(tone)
            }
        }
    } else {
        None
    };

    let encoded = if let Some((kr, kb)) = color.matrix.luma_weights() {
        let levels = color.range.levels(bit_depth);
        let (y, cb, cr) = (
            (sample.y * levels.maximum - levels.luma_black) / levels.luma_span,
            (sample.u * levels.maximum - levels.chroma_center) / levels.chroma_span,
            (sample.v * levels.maximum - levels.chroma_center) / levels.chroma_span,
        );
        let kg = 1.0 - kr - kb;
        [
            y + 2.0 * (1.0 - kr) * cr,
            y - 2.0 * kb * (1.0 - kb) / kg * cb - 2.0 * kr * (1.0 - kr) / kg * cr,
            y + 2.0 * (1.0 - kb) * cb,
        ]
    } else {
        [sample.y, sample.u, sample.v]
    };
    let encoded = encoded.map(|component| component.clamp(0.0, 1.0));
    let linear = match tone {
        Some(tone) if matches!(color.transfer, TransferFunction::Pq | TransferFunction::Hlg) => {
            hdr_to_working(encoded, color.transfer, color.primaries, tone)
        }
        _ => {
            let decoded = encoded.map(decode_sdr);
            if color.primaries == ColorPrimaries::Bt2020 {
                // Wide-gamut SDR: into BT.709 primaries, clipped where they cannot hold it.
                gamut_matrix(ColorPrimaries::Bt2020, ColorPrimaries::Bt709).map(|row| {
                    row.iter()
                        .zip(decoded)
                        .map(|(weight, value)| weight * value)
                        .sum::<f32>()
                        .clamp(0.0, 1.0)
                })
            } else {
                decoded
            }
        }
    };
    let mut output = [0.0; 4];
    for (destination, component) in output[..3].iter_mut().zip(linear) {
        *destination = component * sample.alpha;
    }
    output[3] = sample.alpha;
    Ok((output, diagnostics))
}

/// Encodes linear-light RGB for an SDR output target.
#[must_use]
pub fn encode_linear_rgb(rgb: [f32; 3], transfer: SdrOutputTransfer) -> [f32; 3] {
    rgb.map(|value| {
        let value = value.clamp(0.0, 1.0);
        match transfer {
            SdrOutputTransfer::Srgb => {
                if value <= 0.003_130_8 {
                    12.92 * value
                } else {
                    1.055 * value.powf(1.0 / 2.4) - 0.055
                }
            }
            SdrOutputTransfer::Bt709 => {
                if value < 0.018 {
                    4.5 * value
                } else {
                    1.099 * value.powf(0.45) - 0.099
                }
            }
        }
    })
}

/// Linearizes an SDR component into the display-referred working space.
///
/// Every SDR transfer tag takes the sRGB EOTF: a BT.709 tag names the camera
/// OETF, not how the signal is displayed, and export encodes with the exact
/// inverse, so an import then export without effects is the identity.
fn decode_sdr(value: f32) -> f32 {
    if value <= 0.040_45 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// A premultiplied, sRGB-encoded RGBA storage value with components in the
/// `0..=1` range and no color component above alpha: the values an
/// intermediate texture holds, not linear light.
///
/// [`crate::Renderer::render_solid`] clears to it as given, and a
/// composite's background ([`crate::RenderGraph::add_composite_over`]) is
/// the bottom layer as stored, which a linear-light blend decodes like any
/// other layer. A color meant in linear light is encoded first: mid-grey
/// `#808080` is `0.502`, not its linear `0.216`, which would store as
/// `#373737`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Color {
    /// Red component.
    pub red: f64,
    /// Green component.
    pub green: f64,
    /// Blue component.
    pub blue: f64,
    /// Alpha component.
    pub alpha: f64,
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::*;
    use alpha2_media::{ColorMatrix, ColorPrimaries, ColorRange, DynamicRange};

    fn metadata(matrix: ColorMatrix, range: ColorRange) -> ColorMetadata {
        ColorMetadata {
            primaries: ColorPrimaries::Bt709,
            transfer: TransferFunction::Bt709,
            matrix,
            range,
            dynamic_range: DynamicRange::Sdr,
        }
    }

    #[test]
    fn neutral_black_and_white_match_all_sdr_matrices_and_ranges() {
        for matrix in [
            ColorMatrix::Bt601,
            ColorMatrix::Bt709,
            ColorMatrix::Bt2020Ncl,
        ] {
            for range in [ColorRange::Limited, ColorRange::Full] {
                let neutral = 128.0 / 255.0;
                let black_y = if range == ColorRange::Limited {
                    16.0 / 255.0
                } else {
                    0.0
                };
                let (black, _) = convert_yuv_to_linear_rgba(
                    YuvSample {
                        y: black_y,
                        u: neutral,
                        v: neutral,
                        alpha: 1.0,
                    },
                    8,
                    Some(metadata(matrix, range)),
                    ColorPolicy::RejectHdr,
                )
                .expect("SDR");
                assert!(black[..3].iter().all(|value| value.abs() < 1.0e-5));
                let white_y = if range == ColorRange::Limited {
                    235.0 / 255.0
                } else {
                    1.0
                };
                let (white, _) = convert_yuv_to_linear_rgba(
                    YuvSample {
                        y: white_y,
                        u: neutral,
                        v: neutral,
                        alpha: 0.5,
                    },
                    8,
                    Some(metadata(matrix, range)),
                    ColorPolicy::RejectHdr,
                )
                .expect("SDR");
                assert!(white[..3].iter().all(|value| (*value - 0.5).abs() < 2.0e-4));
            }
        }
    }

    #[test]
    fn wide_gamut_sdr_converts_into_bt709_primaries_and_clips_what_they_cannot_hold() {
        // Every other SDR test declares BT.709 primaries, so only this one runs the BT.2087 branch.
        let bt2020_sdr = ColorMetadata {
            primaries: ColorPrimaries::Bt2020,
            ..metadata(ColorMatrix::Rgb, ColorRange::Full)
        };
        let linear = |encoded: [f32; 3]| {
            let (output, diagnostics) = convert_yuv_to_linear_rgba(
                YuvSample {
                    y: encoded[0],
                    u: encoded[1],
                    v: encoded[2],
                    alpha: 1.0,
                },
                8,
                Some(bt2020_sdr),
                ColorPolicy::RejectHdr,
            )
            .expect("wide-gamut SDR is not HDR");
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            [output[0], output[1], output[2]]
        };
        // ITU-R BT.2087's linear BT.2020 to BT.709 matrix, to its four decimals.
        let bt2087 = [
            [1.6605, -0.5876, -0.0728],
            [-0.1246, 1.1329, -0.0083],
            [-0.0182, -0.1006, 1.1187],
        ];
        // A muted green, inside the BT.709 gamut once converted.
        let encoded = [0.55_f32, 0.6, 0.5];
        let decoded = encoded.map(decode_sdr);
        let expected = bt2087.map(|row| row.iter().zip(decoded).map(|(m, v)| m * v).sum::<f32>());
        assert!(expected.iter().all(|value| (0.0..=1.0).contains(value)));
        let converted = linear(encoded);
        for (got, want) in converted.iter().zip(expected) {
            assert!((got - want).abs() < 2.0e-3, "{converted:?} != {expected:?}");
        }
        assert!(
            (converted[0] - decoded[0]).abs() > 0.01,
            "{converted:?}: the BT.2020 row is not the identity"
        );
        let gray = linear([0.5; 3]);
        assert!(
            gray.iter()
                .all(|value| (value - decode_sdr(0.5)).abs() < 1.0e-4),
            "{gray:?}"
        );
        // BT.2020's green primary lies outside BT.709: red and blue go negative and clip to
        // zero.
        assert_eq!(
            linear([0.0, 1.0, 0.0]).map(f32::to_bits),
            [0.0_f32, 1.0, 0.0].map(f32::to_bits)
        );
        // The same codes tagged BT.709 are taken as they are.
        let (bt709, _) = convert_yuv_to_linear_rgba(
            YuvSample {
                y: encoded[0],
                u: encoded[1],
                v: encoded[2],
                alpha: 1.0,
            },
            8,
            Some(metadata(ColorMatrix::Rgb, ColorRange::Full)),
            ColorPolicy::RejectHdr,
        )
        .expect("SDR");
        assert_eq!(
            [bt709[0], bt709[1], bt709[2]].map(f32::to_bits),
            decoded.map(f32::to_bits)
        );
    }

    #[test]
    fn sdr_decode_is_the_inverse_of_the_srgb_export_encode_for_every_tag() {
        for transfer in [TransferFunction::Bt709, TransferFunction::Srgb] {
            for code in 16_u8..=235 {
                let (linear, _) = convert_yuv_to_linear_rgba(
                    YuvSample {
                        y: f32::from(code) / 255.0,
                        u: 128.0 / 255.0,
                        v: 128.0 / 255.0,
                        alpha: 1.0,
                    },
                    8,
                    Some(ColorMetadata {
                        transfer,
                        ..metadata(ColorMatrix::Bt709, ColorRange::Limited)
                    }),
                    ColorPolicy::RejectHdr,
                )
                .expect("SDR");
                let encoded =
                    encode_linear_rgb([linear[0], linear[1], linear[2]], SdrOutputTransfer::Srgb);
                let expected = (f32::from(code) - 16.0) / 219.0;
                assert!(
                    encoded
                        .iter()
                        .all(|value| (value - expected).abs() * 219.0 < 0.01),
                    "{transfer:?} code {code}: {encoded:?} != {expected}"
                );
            }
        }
    }

    #[test]
    fn rgb_matrix_preserves_encoded_components() {
        let (red, diagnostics) = convert_yuv_to_linear_rgba(
            YuvSample {
                y: 1.0,
                u: 0.0,
                v: 0.0,
                alpha: 1.0,
            },
            8,
            Some(metadata(ColorMatrix::Rgb, ColorRange::Limited)),
            ColorPolicy::RejectHdr,
        )
        .expect("RGB SDR input");
        assert_eq!(diagnostics, [] as [ColorDiagnostic; 0]);
        assert_eq!(
            red.map(f32::to_bits),
            [1.0, 0.0, 0.0, 1.0].map(f32::to_bits)
        );
    }

    #[test]
    fn fallback_and_hdr_are_observable() {
        let (_, diagnostics) = convert_yuv_to_linear_rgba(
            YuvSample {
                y: 16.0 / 255.0,
                u: 128.0 / 255.0,
                v: 128.0 / 255.0,
                alpha: 1.0,
            },
            8,
            None,
            ColorPolicy::RejectHdr,
        )
        .expect("fallback");
        assert_eq!(diagnostics, [ColorDiagnostic::MissingMetadataFallback]);
        let hdr = ColorMetadata {
            dynamic_range: DynamicRange::Hdr,
            ..metadata(ColorMatrix::Bt2020Ncl, ColorRange::Limited)
        };
        assert_eq!(
            convert_yuv_to_linear_rgba(
                YuvSample {
                    y: 0.5,
                    u: 0.5,
                    v: 0.5,
                    alpha: 1.0
                },
                10,
                Some(hdr),
                ColorPolicy::RejectHdr
            ),
            Err(ColorError::UnsupportedHdr)
        );
    }
}

/// How far a premultiplied color component may exceed its alpha: a few
/// `f32` rounding steps near one, far below the finest step any intermediate
/// format stores (`rgba16float`'s 1/2048 near one).
const PREMULTIPLIED_SLACK: f64 = 1e-6;

impl Color {
    /// An opaque black color.
    pub const BLACK: Self = Self::rgba(0.0, 0.0, 0.0, 1.0);

    /// Creates an RGBA color.
    #[must_use]
    pub const fn rgba(red: f64, green: f64, blue: f64, alpha: f64) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }

    /// Whether this is a storage value: every component finite and in
    /// `0..=1`, and, being premultiplied, no color component above alpha.
    /// A component may exceed alpha by [`PREMULTIPLIED_SLACK`], the rounding
    /// of a product computed in `f32`, which no storage format can show.
    pub(crate) fn is_valid(self) -> bool {
        let in_range = [self.red, self.green, self.blue, self.alpha]
            .into_iter()
            .all(|component| component.is_finite() && (0.0..=1.0).contains(&component));
        in_range
            && [self.red, self.green, self.blue]
                .into_iter()
                .all(|component| component <= self.alpha + PREMULTIPLIED_SLACK)
    }

    /// Composites a premultiplied source color over this premultiplied color.
    ///
    /// Both RGB triplets must already be multiplied by their alpha components.
    /// This is the exact Porter-Duff source-over equation used by the GPU blend
    /// state: `out = source + destination * (1 - source_alpha)`.
    #[must_use]
    pub fn source_over(self, source: Self) -> Self {
        let remaining = 1.0 - source.alpha;
        Self::rgba(
            source.red + self.red * remaining,
            source.green + self.green * remaining,
            source.blue + self.blue * remaining,
            source.alpha + self.alpha * remaining,
        )
    }
}
