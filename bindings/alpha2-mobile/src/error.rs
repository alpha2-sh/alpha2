//! The errors every call returns, by the engine's error codes.

use alpha2_media::{DecodeError, EncodeError, EncoderRefusal};

/// One encoder that would not encode, and why.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct EncoderRefusalRecord {
    /// The encoder's name.
    pub encoder: String,
    /// Whether it runs on dedicated hardware.
    pub hardware_accelerated: bool,
    /// The failure class: `absent`, `busy`, `unsuitable` or `unknown`.
    pub kind: String,
    /// What it reported.
    pub reason: String,
}

impl From<&EncoderRefusal> for EncoderRefusalRecord {
    fn from(refusal: &EncoderRefusal) -> Self {
        Self {
            encoder: refusal.encoder.clone(),
            hardware_accelerated: refusal.hardware_accelerated,
            kind: refusal.kind.as_str().to_owned(),
            reason: refusal.reason.clone(),
        }
    }
}

/// Why a call failed. `code()` is the engine's error code, as the desktop host reports it.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum Alpha2Error {
    /// The request or project is invalid (`INVALID_INPUT`).
    #[error("invalid input:\n{}", problems.join("\n"))]
    InvalidInput {
        /// One line per problem.
        problems: Vec<String>,
    },
    /// The project's media is not ready (`CONFORM_FAILED`).
    #[error("{reason}")]
    ConformFailed {
        /// What blocks the export.
        reason: String,
        /// The conform report, as JSON.
        report_json: String,
    },
    /// This device or build cannot do what was asked (`UNSUPPORTED`).
    #[error("{reason}")]
    Unsupported {
        /// What is missing.
        reason: String,
    },
    /// No encoder here would encode the requested codec (`ENCODERS_REFUSED`).
    #[error("{reason}")]
    EncodersRefused {
        /// The summary.
        reason: String,
        /// Each encoder tried, in order.
        refusals: Vec<EncoderRefusalRecord>,
    },
    /// The GPU device could not be created (`GPU_INIT_FAILED`).
    #[error("{reason}")]
    GpuInitFailed {
        /// The cause.
        reason: String,
    },
    /// The output was encoded but could not be moved to its path (`OUTPUT_PUBLISH_FAILED`).
    #[error("{reason}")]
    OutputPublishFailed {
        /// The target, the step that failed and the operating system's error.
        reason: String,
        /// Where the finished file was kept beside the output, if it was; the caller owns it.
        kept_path: Option<String>,
    },
    /// An edit operation refused (`INVALID_INPUT`).
    #[error("{reason}")]
    EditRefused {
        /// Why, in snake case: `invalid_project`, `invalid_request`, `unknown_data`,
        /// `wrong_kind`, `unknown_target`, `empty_target`, `missing_anchor`, `invalid_range`,
        /// `nothing_to_bake`, `correction_not_bakeable`, `matte_source` or `read_through`.
        refusal: String,
        /// What went wrong.
        reason: String,
    },
    /// The call was cancelled (`ABORTED`).
    #[error("alpha2 export was aborted")]
    Aborted,
    /// Any other failure, with its code (`DECODE_FAILED`, `OUTPUT_PUBLISH_FAILED`, ...).
    #[error("{reason}")]
    Failed {
        /// The engine's error code.
        code: String,
        /// The cause.
        reason: String,
    },
}

impl Alpha2Error {
    /// The engine's error code.
    #[must_use]
    pub fn code(&self) -> &str {
        match self {
            Self::InvalidInput { .. } | Self::EditRefused { .. } => "INVALID_INPUT",
            Self::ConformFailed { .. } => "CONFORM_FAILED",
            Self::Unsupported { .. } => "UNSUPPORTED",
            Self::EncodersRefused { .. } => "ENCODERS_REFUSED",
            Self::GpuInitFailed { .. } => "GPU_INIT_FAILED",
            Self::OutputPublishFailed { .. } => "OUTPUT_PUBLISH_FAILED",
            Self::Aborted => "ABORTED",
            Self::Failed { code, .. } => code,
        }
    }

    pub(crate) fn invalid(problem: impl Into<String>) -> Self {
        Self::InvalidInput {
            problems: vec![problem.into()],
        }
    }
}

impl From<DecodeError> for Alpha2Error {
    fn from(error: DecodeError) -> Self {
        match error {
            DecodeError::Cancelled => Self::Aborted,
            DecodeError::Unsupported(reason) => Self::Unsupported { reason },
            error => Self::Failed {
                code: "DECODE_FAILED".into(),
                reason: error.to_string(),
            },
        }
    }
}

impl From<EncodeError> for Alpha2Error {
    fn from(error: EncodeError) -> Self {
        match error {
            EncodeError::Cancelled => Self::Aborted,
            EncodeError::EncodersRefused { message, refusals } => Self::EncodersRefused {
                reason: message,
                refusals: refusals.iter().map(EncoderRefusalRecord::from).collect(),
            },
            EncodeError::Publish { message, kept } => Self::OutputPublishFailed {
                reason: message,
                kept_path: kept.map(|path| path.to_string_lossy().into_owned()),
            },
            EncodeError::OutputInUse(reason) => Self::Failed {
                code: "OUTPUT_IN_USE".into(),
                reason,
            },
            EncodeError::Output(reason) => Self::Failed {
                code: "OUTPUT_IO_FAILED".into(),
                reason,
            },
            error => Self::Failed {
                code: "ENCODE_FAILED".into(),
                reason: error.to_string(),
            },
        }
    }
}
