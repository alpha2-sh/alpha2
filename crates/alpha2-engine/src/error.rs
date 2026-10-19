use std::fmt;

/// Failure produced by an engine runner.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EngineError {
    /// Timeline evaluation failed.
    Evaluate(alpha2_eval::EvalError),
    /// GPU rendering failed.
    Render(alpha2_render::RenderError),
    /// A quality profile contains an unusable value.
    InvalidQualityProfile,
    /// A frame index or scaled output size cannot be represented.
    OutOfRange,
    /// An exporter requires a non-empty timeline interval.
    EmptyExportRange,
    /// An export schedule exceeds the bounded frame budget.
    ExportTooLarge {
        /// Requested frame count.
        frames: u64,
        /// Largest supported frame count.
        maximum: u64,
    },
    /// A caption or a text generator could not be rasterized or uploaded.
    Text(crate::TextError),
    /// An audio renderer received an unsupported layout, rate, or block size.
    InvalidAudioConfiguration,
    /// A loudness normalization gain is not finite, or its linear factor is
    /// not a finite, positive 32-bit float.
    InvalidNormalizationGain,
    /// The project draws an effect the renderer has not registered, or
    /// whose registration failed.
    UnregisteredEffect {
        /// The effect's id.
        effect: String,
        /// Its version.
        version: u32,
    },
    /// An export step was given another snapshot than the one its exporter
    /// was made for.
    ExportProjectChanged,
    /// A 4:2:0 export was asked for an output with an odd width or height.
    ///
    /// Its chroma planes are half size, and H.264 and HEVC crop in two-pixel
    /// steps, so the bitstream cannot carry an odd size. The project canvas
    /// may be odd; making the output even is the host's choice.
    OddExportDimensions {
        /// Output width.
        width: u32,
        /// Output height.
        height: u32,
        /// The nearest even size, each odd dimension one pixel smaller
        /// (larger for a one-pixel dimension).
        suggested: (u32, u32),
    },
}

impl fmt::Display for EngineError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Evaluate(error) => write!(formatter, "timeline evaluation failed: {error}"),
            Self::Render(error) => write!(formatter, "rendering failed: {error}"),
            Self::InvalidQualityProfile => formatter.write_str("quality profile is invalid"),
            Self::OutOfRange => formatter.write_str("timeline or output size is out of range"),
            Self::EmptyExportRange => formatter.write_str("export range must not be empty"),
            Self::ExportTooLarge { frames, maximum } => write!(
                formatter,
                "export requests {frames} frames; the maximum is {maximum}"
            ),
            Self::Text(error) => write!(formatter, "{error}"),
            Self::InvalidAudioConfiguration => {
                formatter.write_str("audio channel layout, sample rate, or block size is invalid")
            }
            Self::InvalidNormalizationGain => formatter.write_str(
                "loudness normalization gain is not finite or past what a 32-bit float factor holds",
            ),
            Self::UnregisteredEffect { effect, version } => write!(
                formatter,
                "the project draws effect {effect}@{version}, which is not registered"
            ),
            Self::ExportProjectChanged => formatter.write_str(
                "an export step was given another project than the export was started with",
            ),
            Self::OddExportDimensions {
                width,
                height,
                suggested: (suggested_width, suggested_height),
            } => write!(
                formatter,
                "4:2:0 video export must have even dimensions, but the output is \
                 {width}x{height}; the nearest even size is {suggested_width}x{suggested_height}"
            ),
        }
    }
}

impl std::error::Error for EngineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Evaluate(error) => Some(error),
            Self::Render(error) => Some(error),
            _ => None,
        }
    }
}

impl From<alpha2_eval::EvalError> for EngineError {
    fn from(error: alpha2_eval::EvalError) -> Self {
        Self::Evaluate(error)
    }
}

impl From<alpha2_render::RenderError> for EngineError {
    fn from(error: alpha2_render::RenderError) -> Self {
        Self::Render(error)
    }
}
