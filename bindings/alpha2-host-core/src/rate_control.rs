//! Rate-control vocabulary shared by every encoder backend.

/// A way of controlling an encoder's rate, independent of family spelling.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum RateControlMechanism {
    /// Constant bitrate.
    ConstantBitrate,
    /// Variable (average) bitrate.
    VariableBitrate,
    /// Constant quantizer: `FFmpeg`'s `cqp`/`constqp`, the family of `crf`.
    ConstantQuantizer,
}

impl RateControlMechanism {
    /// Every mechanism, in the order the probe reports them.
    pub const ALL: [Self; 3] = [
        Self::ConstantBitrate,
        Self::VariableBitrate,
        Self::ConstantQuantizer,
    ];

    /// Stable identifier for diagnostics.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ConstantBitrate => "cbr",
            Self::VariableBitrate => "vbr",
            Self::ConstantQuantizer => "cqp",
        }
    }
}

/// Which form of a resolved rate control an encoder actually ran.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum AppliedRateControl {
    /// The constant quantizer.
    Quantizer,
    /// The quantizer mapped onto the encoder's own quality scale (`VideoToolbox`'s 0-1 `Quality`).
    Quality,
    /// The bitrate: none was asked for, or the encoder refused the quantizer.
    Bitrate,
}

impl AppliedRateControl {
    /// Stable identifier for the export result.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Quantizer => "quantizer",
            Self::Quality => "quality",
            Self::Bitrate => "bitrate",
        }
    }
}

/// `VideoToolbox` quality (`kVTCompressionPropertyKey_Quality`, 0-1, higher better) per H.264
/// quantizer at the levels' quantizers (`alpha2_media::level_quantizer`).
///
/// Each point holds `h264_videotoolbox` on Apple silicon within 0.5 dB PSNR-Y of `OpenH264` at the
/// same level on a complex 1080p30 project (40.6 to 52.0 dB), at 0.66x-0.85x its bitrate. Grainy
/// content needs about 0.05-0.10 more for the same match.
const VIDEOTOOLBOX_QUALITY: [(u32, f64); 5] =
    [(23, 0.65), (27, 0.57), (31, 0.50), (35, 0.40), (41, 0.25)];

/// The `VideoToolbox` quality for an H.264 `quantizer`: a measured table interpolated,
/// extrapolated along its end segments, rounded to hundredths and clamped to 0.01-1.
#[must_use]
pub fn videotoolbox_quality(quantizer: u32) -> f64 {
    let table = &VIDEOTOOLBOX_QUALITY;
    let segment = table
        .windows(2)
        .find(|pair| quantizer <= pair[1].0)
        .unwrap_or(&table[table.len() - 2..]);
    let ((q0, v0), (q1, v1)) = (segment[0], segment[1]);
    let position = (f64::from(quantizer) - f64::from(q0)) / (f64::from(q1) - f64::from(q0));
    let quality = v0 + (v1 - v0) * position;
    (quality * 100.0).round().clamp(1.0, 100.0) / 100.0
}
