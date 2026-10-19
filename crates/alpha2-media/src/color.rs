use serde::{Deserialize, Serialize};

/// Host-independent video color description.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ColorMetadata {
    /// RGB chromaticity standard.
    pub primaries: ColorPrimaries,
    /// Nonlinear transfer curve.
    pub transfer: TransferFunction,
    /// YUV-to-RGB coefficient family, or RGB for already converted sources.
    pub matrix: ColorMatrix,
    /// Encoded component range.
    pub range: ColorRange,
    /// SDR or HDR signal classification: HDR exactly when `transfer` is PQ
    /// or HLG ([`Self::is_consistent`]).
    pub dynamic_range: DynamicRange,
}

impl ColorMetadata {
    /// What every SDR export is encoded as and tagged with: BT.709 primaries,
    /// transfer, and matrix, in limited range.
    ///
    /// The working space is display-referred: export encodes it
    /// with the sRGB curve, the exact inverse of how every SDR input is
    /// linearized, and tags the result BT.709, as SDR video is. Every export
    /// path, native and browser, packs and tags from this one value.
    pub const EXPORT: Self = Self {
        primaries: ColorPrimaries::Bt709,
        transfer: TransferFunction::Bt709,
        matrix: ColorMatrix::Bt709,
        range: ColorRange::Limited,
        dynamic_range: DynamicRange::Sdr,
    };

    /// What a source that declares no color is decoded as: BT.709 limited
    /// SDR. The fallback is reported: the renderer's
    /// `ColorDiagnostic::MissingMetadataFallback` for a frame without a
    /// color, and conform's `undeclared_color` for a video whose color
    /// neither the project nor its media declares.
    pub const UNDECLARED: Self = Self::EXPORT;

    /// Whether the signal is HDR: carried by a PQ or HLG transfer, or
    /// classified so. The one rule every consumer decides HDR by: lint,
    /// conform, the renderer, and the native decoders and encoders. For a
    /// consistent value ([`Self::is_consistent`]) the two fields agree.
    #[must_use]
    pub const fn is_hdr(self) -> bool {
        matches!(self.dynamic_range, DynamicRange::Hdr)
            || matches!(self.transfer, TransferFunction::Pq | TransferFunction::Hlg)
    }

    /// Whether `dynamic_range` says what `transfer` does: HDR exactly when
    /// the transfer is PQ or HLG. A project declaring or recording a color
    /// that is not is refused, so no consumer has to choose between them.
    #[must_use]
    pub const fn is_consistent(self) -> bool {
        matches!(self.dynamic_range, DynamicRange::Hdr)
            == matches!(self.transfer, TransferFunction::Pq | TransferFunction::Hlg)
    }
}

impl Default for ColorMetadata {
    /// [`Self::UNDECLARED`].
    fn default() -> Self {
        Self::UNDECLARED
    }
}

/// How rendered RGB is written as 8-bit 4:2:0 YUV: its color coding, and
/// where its chroma samples sit relative to the luma grid.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct YuvEncoding {
    /// Matrix, range, and the tags the output carries.
    pub color: ColorMetadata,
    /// Where each chroma sample sits, so the pack filters for that point.
    pub chroma: crate::ChromaLocation,
}

impl YuvEncoding {
    /// `color` with chroma sited at `chroma`.
    #[must_use]
    pub const fn new(color: ColorMetadata, chroma: crate::ChromaLocation) -> Self {
        Self { color, chroma }
    }

    /// What an export to `codec` writes: [`ColorMetadata::EXPORT`], with the
    /// chroma where the codec implies it ([`crate::VideoCodec::chroma_location`]).
    #[must_use]
    pub const fn export(codec: crate::VideoCodec) -> Self {
        Self::new(ColorMetadata::EXPORT, codec.chroma_location())
    }
}

/// RGB primary chromaticities a source or export declares.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ColorPrimaries {
    /// SD video primaries.
    Bt601,
    /// HD video and sRGB primaries.
    Bt709,
    /// Wide-gamut BT.2020 primaries, normally indicating HDR input.
    Bt2020,
}

/// Encoded transfer curve.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum TransferFunction {
    /// BT.709 opto-electronic transfer function.
    Bt709,
    /// IEC sRGB transfer function.
    Srgb,
    /// Perceptual quantizer HDR transfer function.
    Pq,
    /// Hybrid log-gamma HDR transfer function.
    Hlg,
}

/// Coefficients used to decode source components.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ColorMatrix {
    /// SD BT.601 YUV coefficients.
    Bt601,
    /// HD BT.709 YUV coefficients.
    Bt709,
    /// BT.2020 non-constant-luminance YUV coefficients.
    Bt2020Ncl,
    /// Source components are already RGB.
    Rgb,
}

impl ColorMatrix {
    /// The red and blue luma weights `(Kr, Kb)` of a YCbCr matrix, or `None`
    /// for components that are already RGB.
    ///
    /// These are the only definition of the coefficients: import, export, and
    /// the CPU reference all derive their matrices from them. Green's weight
    /// is `1 - Kr - Kb`.
    #[must_use]
    pub const fn luma_weights(self) -> Option<(f32, f32)> {
        match self {
            Self::Bt601 => Some((0.299, 0.114)),
            Self::Bt709 => Some((0.2126, 0.0722)),
            Self::Bt2020Ncl => Some((0.2627, 0.0593)),
            Self::Rgb => None,
        }
    }
}

/// Where a YCbCr range puts a bit depth's codes (ITU-T H.273).
///
/// Luma `Y'` is `luma_black + luma_span * Y` for `Y` in `0..=1`; each chroma
/// component is `chroma_center + chroma_span * C` for `C` in `-0.5..=0.5`.
/// A sampled unorm texture reads a code as `code / maximum`.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct YcbcrLevels {
    /// The largest code, `2^depth - 1`.
    pub maximum: f32,
    /// Black's luma code.
    pub luma_black: f32,
    /// Codes from black to white.
    pub luma_span: f32,
    /// Neutral chroma's code.
    pub chroma_center: f32,
    /// Codes across chroma's full swing, `-0.5` to `0.5`.
    pub chroma_span: f32,
}

impl ColorRange {
    /// Where this range puts `bit_depth`-bit codes: limited range scales 16,
    /// 219, 128, and 224 with the depth; full range spans every code, with
    /// neutral chroma at `2^(depth - 1)`, not at one half.
    ///
    /// This is the only definition of the levels: import, export, and the
    /// CPU reference all take them from here.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // Every code of 16 bits is exact in f32.
    pub fn levels(self, bit_depth: u8) -> YcbcrLevels {
        let depth = u32::from(bit_depth.clamp(8, 16));
        let maximum = ((1_u32 << depth) - 1) as f32;
        let scale = (1_u32 << (depth - 8)) as f32;
        match self {
            Self::Limited => YcbcrLevels {
                maximum,
                luma_black: 16.0 * scale,
                luma_span: 219.0 * scale,
                chroma_center: 128.0 * scale,
                chroma_span: 224.0 * scale,
            },
            Self::Full => YcbcrLevels {
                maximum,
                luma_black: 0.0,
                luma_span: maximum,
                chroma_center: (1_u32 << (depth - 1)) as f32,
                chroma_span: maximum,
            },
        }
    }
}

/// Nominal encoded component range.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ColorRange {
    /// Studio/video range.
    Limited,
    /// Full component range.
    Full,
}

/// Whether a signal is standard or high dynamic range. It follows the
/// transfer: `hdr` exactly when the transfer is PQ or HLG, and a color
/// whose two fields disagree is refused.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum DynamicRange {
    /// Standard dynamic range: a BT.709 or sRGB transfer.
    Sdr,
    /// High dynamic range: a PQ or HLG transfer, which a project tone-maps
    /// into the SDR working space or refuses, as its color pipeline says.
    Hdr,
}

#[cfg(test)]
// The levels are exact codes: nothing here is approximately equal.
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    #[test]
    fn levels_scale_the_limited_range_with_the_bit_depth() {
        let eight = ColorRange::Limited.levels(8);
        assert_eq!(
            [
                eight.maximum,
                eight.luma_black,
                eight.luma_span,
                eight.chroma_span
            ],
            [255.0, 16.0, 219.0, 224.0]
        );
        let ten = ColorRange::Limited.levels(10);
        assert_eq!(
            [
                ten.maximum,
                ten.luma_black,
                ten.luma_span,
                ten.chroma_center
            ],
            [1023.0, 64.0, 876.0, 512.0]
        );
    }

    #[test]
    fn full_range_centres_chroma_on_the_middle_code_not_on_one_half() {
        let eight = ColorRange::Full.levels(8);
        assert_eq!(eight.chroma_center, 128.0);
        assert!(eight.chroma_center / eight.maximum > 0.5);
        assert_eq!(ColorRange::Full.levels(10).chroma_center, 512.0);
    }

    #[test]
    fn the_matrices_weigh_luma_as_their_standards_do() {
        for matrix in [
            ColorMatrix::Bt601,
            ColorMatrix::Bt709,
            ColorMatrix::Bt2020Ncl,
        ] {
            let (kr, kb) = matrix.luma_weights().expect("YCbCr");
            assert!(kr > 0.0 && kb > 0.0 && kr + kb < 1.0);
        }
        assert_eq!(ColorMatrix::Rgb.luma_weights(), None);
        assert_eq!(ColorMetadata::default(), ColorMetadata::EXPORT);
    }
}
