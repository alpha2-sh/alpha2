//! The protocol code of an engine failure, decided by the error's variant.
//!
//! The native host and the wasm binding share these codes. The code comes from the error variant,
//! never from message text, which can carry user-chosen IDs and paths. Errors of unknown types
//! are coded by the binding, defaulting to `ALPHA2_ERROR`.

use std::error::Error;

use alpha2_eval::{EvalError, PlanError};
use alpha2_media::{DecodeError, EncodeError, GpuFault};
use alpha2_render::{ColorError, GraphError, LutParseError, RenderError};
use alpha2_text::TextRasterError;

use crate::{EngineError, TextError};

/// A failure's protocol code.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum ErrorCode {
    /// The project, a range, a rate, a size, or another argument is invalid.
    InvalidInput,
    /// An effect asset, font, or frame the render needs is absent.
    MissingInput,
    /// A composition graph has an unconnected output or input.
    IncompleteComposition,
    /// A runtime effect's definition, parameters, or shader failed.
    EffectError,
    /// Encoding or muxing failed.
    EncodeError,
    /// The GPU device was lost, or no longer runs submitted work.
    GpuDeviceLost,
    /// The device was lost to this work: a submission ran for most of the OS GPU watchdog, or the
    /// platform reported a reset for a long command. Retrying will hang the GPU again.
    GpuDeviceHung,
    /// GPU work did not complete within the wait timeout.
    GpuTimeout,
    /// A failure nothing classified.
    Alpha2Error,
}

impl ErrorCode {
    /// Every code, for tables and tests.
    pub const ALL: [Self; 9] = [
        Self::InvalidInput,
        Self::MissingInput,
        Self::IncompleteComposition,
        Self::EffectError,
        Self::EncodeError,
        Self::GpuDeviceLost,
        Self::GpuDeviceHung,
        Self::GpuTimeout,
        Self::Alpha2Error,
    ];

    /// The code as the protocol spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "INVALID_INPUT",
            Self::MissingInput => "MISSING_INPUT",
            Self::IncompleteComposition => "INCOMPLETE_COMPOSITION",
            Self::EffectError => "EFFECT_ERROR",
            Self::EncodeError => "ENCODE_ERROR",
            Self::GpuDeviceLost => "GPU_DEVICE_LOST",
            Self::GpuDeviceHung => "GPU_DEVICE_HUNG",
            Self::GpuTimeout => "GPU_TIMEOUT",
            Self::Alpha2Error => "ALPHA2_ERROR",
        }
    }

    /// The code of the first error in `error`'s source chain with a known type.
    #[must_use]
    pub fn of(error: &(dyn Error + 'static)) -> Option<Self> {
        let mut cause = Some(error);
        while let Some(error) = cause {
            if let Some(code) = Self::of_one(error) {
                return Some(code);
            }
            cause = error.source();
        }
        None
    }

    fn of_one(error: &(dyn Error + 'static)) -> Option<Self> {
        if let Some(error) = error.downcast_ref::<EngineError>() {
            return Some(error.code());
        }
        if let Some(error) = error.downcast_ref::<EvalError>() {
            return Some(eval_code(error));
        }
        if let Some(error) = error.downcast_ref::<RenderError>() {
            return Some(render_code(*error));
        }
        if let Some(error) = error.downcast_ref::<TextError>() {
            return Some(text_error_code(error));
        }
        if let Some(error) = error.downcast_ref::<TextRasterError>() {
            return Some(text_code(error));
        }
        if let Some(error) = error.downcast_ref::<GraphError>() {
            return Some(match error {
                GraphError::Render(error) => render_code(*error),
                _ => Self::InvalidInput,
            });
        }
        // GPU call faults are read first by the binding (`gpu_fault_of`).
        if let Some(error) = error.downcast_ref::<DecodeError>() {
            return Some(match error {
                DecodeError::OutOfRange => Self::InvalidInput,
                _ => Self::Alpha2Error,
            });
        }
        if error.is::<EncodeError>() {
            return Some(Self::EncodeError);
        }
        let invalid = error.is::<alpha2_model::ModelError>()
            || error.is::<alpha2_model::ProjectLoadError>()
            || error.is::<alpha2_time::TimeRangeError>()
            || error.is::<alpha2_time::FrameRateError>()
            || error.is::<alpha2_media::RateControlError>()
            || error.is::<ColorError>()
            || error.is::<LutParseError>();
        invalid.then_some(Self::InvalidInput)
    }
}

/// The GPU error class of the first [`RenderError::Gpu`] in `error`'s source chain. Out of memory
/// and validation have no protocol code; failure reports classify them with this.
#[must_use]
pub fn gpu_error_of(error: &(dyn Error + 'static)) -> Option<alpha2_gpu::GpuErrorKind> {
    let render = |error: &RenderError| match error {
        RenderError::Gpu { kind } => Some(*kind),
        _ => None,
    };
    let mut cause = Some(error);
    while let Some(error) = cause {
        let kind = if let Some(error) = error.downcast_ref::<RenderError>() {
            render(error)
        } else if let Some(EngineError::Render(error)) = error.downcast_ref::<EngineError>() {
            render(error)
        } else if let Some(GraphError::Render(error)) = error.downcast_ref::<GraphError>() {
            render(error)
        } else {
            None
        };
        if kind.is_some() {
            return kind;
        }
        cause = error.source();
    }
    None
}

/// The device fault reported for the first failed GPU call in `error`'s source chain (renderer
/// synchronization, decoder or encoder). Failure reports classify memory faults with this.
#[must_use]
pub fn gpu_fault_of(error: &(dyn Error + 'static)) -> Option<GpuFault> {
    let render = |error: &RenderError| match error {
        RenderError::ExternalTextureSync(fault) => *fault,
        _ => None,
    };
    let mut cause = Some(error);
    while let Some(error) = cause {
        let fault = if let Some(error) = error.downcast_ref::<RenderError>() {
            render(error)
        } else if let Some(EngineError::Render(error)) = error.downcast_ref::<EngineError>() {
            render(error)
        } else if let Some(GraphError::Render(error)) = error.downcast_ref::<GraphError>() {
            render(error)
        } else if let Some(EncodeError::Gpu { fault, .. }) = error.downcast_ref::<EncodeError>() {
            Some(*fault)
        } else if let Some(DecodeError::Gpu { fault, .. }) = error.downcast_ref::<DecodeError>() {
            Some(*fault)
        } else {
            None
        };
        if fault.is_some() {
            return fault;
        }
        cause = error.source();
    }
    None
}

impl EngineError {
    /// This failure's protocol code.
    #[must_use]
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Evaluate(error) => eval_code(error),
            Self::Render(error) => render_code(*error),
            Self::Text(error) => text_error_code(error),
            Self::InvalidQualityProfile
            | Self::OutOfRange
            | Self::EmptyExportRange
            | Self::ExportTooLarge { .. }
            | Self::InvalidAudioConfiguration
            | Self::InvalidNormalizationGain
            | Self::OddExportDimensions { .. } => ErrorCode::InvalidInput,
            // The caller's mistake, not the project's.
            Self::ExportProjectChanged => ErrorCode::Alpha2Error,
            Self::UnregisteredEffect { .. } => ErrorCode::EffectError,
        }
    }
}

fn text_error_code(error: &TextError) -> ErrorCode {
    match error {
        TextError::Raster(error) => text_code(error),
        TextError::Render(error) => render_code(*error),
    }
}

fn text_code(error: &TextRasterError) -> ErrorCode {
    match error {
        TextRasterError::NoFonts => ErrorCode::MissingInput,
        _ => ErrorCode::InvalidInput,
    }
}

fn eval_code(error: &EvalError) -> ErrorCode {
    match error {
        EvalError::IncompleteComposition { reason, .. } => match reason {
            PlanError::NoOutput
            | PlanError::MissingInput { .. }
            | PlanError::UnboundGroupInput { .. }
            | PlanError::ClipWithoutClip { .. } => ErrorCode::IncompleteComposition,
            PlanError::NestedTooDeep { .. } | PlanError::TooLarge => ErrorCode::InvalidInput,
        },
        EvalError::InvalidProject(_) | EvalError::UnmappableClip { .. } => ErrorCode::InvalidInput,
        // An index built for another project is the caller's mistake.
        _ => ErrorCode::Alpha2Error,
    }
}

fn render_code(error: RenderError) -> ErrorCode {
    match error {
        RenderError::ZeroDimension
        | RenderError::DimensionTooLarge
        | RenderError::InvalidColor
        | RenderError::InvalidVisual
        | RenderError::InvalidImageData
        | RenderError::InvalidGraph
        | RenderError::UnsupportedHdr => ErrorCode::InvalidInput,
        RenderError::InvalidEffectParameter
        | RenderError::UnknownEffect
        | RenderError::InvalidEffectParams
        | RenderError::InvalidEffectResource
        | RenderError::EffectResourceKindMismatch
        | RenderError::MissingEffectInput => ErrorCode::EffectError,
        RenderError::MissingEffectResource => ErrorCode::MissingInput,
        RenderError::GpuTimeout | RenderError::ExternalTextureSync(Some(GpuFault::Timeout)) => {
            ErrorCode::GpuTimeout
        }
        RenderError::GpuWorkDropped
        | RenderError::GpuDeviceLost
        | RenderError::ExternalTextureSync(Some(GpuFault::DeviceLost)) => ErrorCode::GpuDeviceLost,
        RenderError::GpuDeviceHung { .. }
        | RenderError::ExternalTextureSync(Some(GpuFault::DeviceHung)) => ErrorCode::GpuDeviceHung,
        // A GPU rejection is the engine's failure, not the caller's input.
        _ => ErrorCode::Alpha2Error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_is_the_variants_whatever_its_message_says() {
        let odd = EngineError::OddExportDimensions {
            width: 65,
            height: 49,
            suggested: (64, 48),
        };
        assert_eq!(odd.code(), ErrorCode::InvalidInput);
        assert_eq!(ErrorCode::of(&odd), Some(ErrorCode::InvalidInput));
        let too_large = EngineError::ExportTooLarge {
            frames: 2,
            maximum: 1,
        };
        assert_eq!(too_large.code(), ErrorCode::InvalidInput);
        let no_fonts = EngineError::Text(TextError::Raster(TextRasterError::NoFonts));
        assert_eq!(no_fonts.code(), ErrorCode::MissingInput);
        assert_eq!(
            EngineError::Render(RenderError::GpuWorkDropped).code(),
            ErrorCode::GpuDeviceLost
        );
        assert_eq!(
            EngineError::Render(RenderError::GpuDeviceLost).code(),
            ErrorCode::GpuDeviceLost
        );
        // A rejected frame is not a lost device, whatever the device said about it.
        for kind in [
            alpha2_gpu::GpuErrorKind::OutOfMemory,
            alpha2_gpu::GpuErrorKind::Validation,
            alpha2_gpu::GpuErrorKind::Internal,
        ] {
            let rejected = EngineError::Render(RenderError::Gpu { kind });
            assert_eq!(rejected.code(), ErrorCode::Alpha2Error);
            assert_eq!(gpu_error_of(&rejected), Some(kind));
            assert_eq!(
                gpu_error_of(&GraphError::Render(RenderError::Gpu { kind })),
                Some(kind)
            );
        }
        assert_eq!(gpu_error_of(&RenderError::GpuWorkDropped), None);
        assert_eq!(
            ErrorCode::of(&RenderError::GpuTimeout),
            Some(ErrorCode::GpuTimeout)
        );
        assert_eq!(
            ErrorCode::of(&RenderError::UnsupportedHdr),
            Some(ErrorCode::InvalidInput)
        );
        let model = alpha2_model::ModelError::UnsupportedSchemaVersion { found: 0 };
        assert_eq!(ErrorCode::of(&model), Some(ErrorCode::InvalidInput));
        assert_eq!(ErrorCode::of(&std::io::Error::other("device lost")), None);
    }

    #[test]
    fn media_errors_take_the_code_their_variant_names_whatever_their_words() {
        let words = "missing shader: GPU device lost, invalid encode".to_owned();
        for (error, code) in [
            (
                &DecodeError::Adapter(words.clone()) as &(dyn Error + 'static),
                ErrorCode::Alpha2Error,
            ),
            (
                &DecodeError::Unsupported(words.clone()),
                ErrorCode::Alpha2Error,
            ),
            (&DecodeError::Cancelled, ErrorCode::Alpha2Error),
            (&DecodeError::OutOfRange, ErrorCode::InvalidInput),
            (&EncodeError::Adapter(words.clone()), ErrorCode::EncodeError),
            (&EncodeError::Cancelled, ErrorCode::EncodeError),
            (&EncodeError::InvalidTimestamp, ErrorCode::EncodeError),
        ] {
            assert_eq!(ErrorCode::of(error), Some(code), "{error}");
        }
    }

    #[test]
    fn every_code_has_its_own_name() {
        let mut names: Vec<_> = ErrorCode::ALL.iter().map(|code| code.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ErrorCode::ALL.len());
    }
}
