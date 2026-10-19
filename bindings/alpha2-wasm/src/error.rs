//! The binding's failures: each with the code the place that refuses
//! decides, and `fatal` only when the binding can render no more.
//!
//! A code is never read from a message here. A message carries source IDs
//! and browser text a caller chose, so a refusal that names a source called
//! `missing-shot` is still the refusal it was. Engine errors take the code
//! their type names ([`alpha2_engine::ErrorCode::of`]), the native host's
//! code for the same failure; everything the binding refuses itself names
//! its code where it refuses.

use alpha2_engine::ErrorCode;

/// A binding failure's code.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum Code {
    /// A code the native host answers too.
    Engine(ErrorCode),
    /// A call the binding's state does not allow: no project, no export, no
    /// canvas.
    InvalidState,
    /// A store's bound refuses an upload; what it held stays.
    ResourceLimit,
    /// The browser offers no GPU device to make a binding on.
    GpuInitFailed,
}

impl Code {
    pub(crate) const INVALID_INPUT: Self = Self::Engine(ErrorCode::InvalidInput);
    pub(crate) const MISSING_INPUT: Self = Self::Engine(ErrorCode::MissingInput);
    pub(crate) const EFFECT_ERROR: Self = Self::Engine(ErrorCode::EffectError);
    pub(crate) const GPU_DEVICE_LOST: Self = Self::Engine(ErrorCode::GpuDeviceLost);
    pub(crate) const GPU_TIMEOUT: Self = Self::Engine(ErrorCode::GpuTimeout);
    /// A failure of the binding's own steps, not of the caller's input.
    pub(crate) const INTERNAL: Self = Self::Engine(ErrorCode::Alpha2Error);

    /// The code as `Alpha2Error.code` spells it.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Engine(code) => code.as_str(),
            Self::InvalidState => "INVALID_STATE",
            Self::ResourceLimit => "RESOURCE_LIMIT",
            Self::GpuInitFailed => "GPU_INIT_FAILED",
        }
    }

    /// Whether the binding that answered can render no more: its device or
    /// context is lost, and its owner must make it again. Every other
    /// failure leaves it usable. A wasm trap is the other fatal failure, but
    /// a trapped module answers nothing, so its host classifies that.
    pub(crate) const fn fatal(self) -> bool {
        matches!(self, Self::Engine(ErrorCode::GpuDeviceLost))
    }
}

/// An error type whose code the engine decides by its variant: the only
/// errors [`BindingError::typed`] takes, so a type the engine does not know
/// cannot be answered `ALPHA2_ERROR` by accident.
pub(crate) trait EngineTyped: std::error::Error + 'static {}

impl EngineTyped for alpha2_engine::EngineError {}
impl EngineTyped for alpha2_engine::TextError {}
impl EngineTyped for alpha2_eval::EvalError {}
impl EngineTyped for alpha2_render::RenderError {}
impl EngineTyped for alpha2_render::GraphError {}
impl EngineTyped for alpha2_render::ColorError {}
impl EngineTyped for alpha2_render::LutParseError {}
impl EngineTyped for alpha2_text::TextRasterError {}
impl EngineTyped for alpha2_model::ModelError {}
impl EngineTyped for alpha2_model::ProjectLoadError {}
impl EngineTyped for alpha2_time::TimeRangeError {}
impl EngineTyped for alpha2_time::FrameRateError {}
impl EngineTyped for alpha2_media::RateControlError {}

/// A failure the binding answers with: its code and its message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BindingError {
    pub(crate) code: Code,
    pub(crate) message: String,
}

impl BindingError {
    pub(crate) fn new(code: Code, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// An engine error, with the code its variant names.
    pub(crate) fn typed<E: EngineTyped>(error: &E) -> Self {
        Self::new(
            ErrorCode::of(error).map_or(Code::INTERNAL, Code::Engine),
            error.to_string(),
        )
    }

    pub(crate) const fn fatal(&self) -> bool {
        self.code.fatal()
    }
}

#[cfg(target_arch = "wasm32")]
impl From<BindingError> for wasm_bindgen::JsValue {
    /// An `Error` named `Alpha2Error`, with `code` and `fatal`.
    fn from(error: BindingError) -> Self {
        let value = web_sys::js_sys::Error::new(&error.message);
        value.set_name("Alpha2Error");
        let _ = web_sys::js_sys::Reflect::set(
            value.as_ref(),
            &Self::from_str("code"),
            &Self::from_str(error.code.as_str()),
        );
        let _ = web_sys::js_sys::Reflect::set(
            value.as_ref(),
            &Self::from_str("fatal"),
            &Self::from_bool(error.fatal()),
        );
        value.into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alpha2_eval::{EvalError, PlanError};
    use alpha2_model::{ClipId, CompositionId, NodeId};
    use alpha2_render::RenderError;

    #[test]
    fn only_a_lost_device_is_fatal() {
        // A failure that leaves the device usable must not be fatal, or a host drops a binding it
        // could still use.
        let codes = [
            Code::INVALID_INPUT,
            Code::MISSING_INPUT,
            Code::Engine(ErrorCode::IncompleteComposition),
            Code::EFFECT_ERROR,
            Code::Engine(ErrorCode::EncodeError),
            Code::GPU_TIMEOUT,
            Code::INTERNAL,
            Code::InvalidState,
            Code::ResourceLimit,
            Code::GpuInitFailed,
        ];
        for code in codes {
            assert!(!code.fatal(), "{}", code.as_str());
        }
        assert!(Code::GPU_DEVICE_LOST.fatal());
        assert_eq!(ErrorCode::ALL.len() + 3, codes.len() + 1);
        let mut names: Vec<_> = codes
            .iter()
            .chain([&Code::GPU_DEVICE_LOST])
            .map(|code| code.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), codes.len() + 1);
    }

    #[test]
    fn every_incomplete_graph_is_incomplete_whatever_its_ids_say() {
        // The code follows the variant, so every plan error is incomplete, not only those whose
        // message reads "is not connected" or "has no output".
        let id = |value: &str| NodeId::new(value).expect("id");
        for reason in [
            PlanError::NoOutput,
            PlanError::MissingInput {
                node: id("device-lost-blur"),
                input: "input".into(),
            },
            PlanError::UnboundGroupInput {
                node: id("missing-input"),
            },
            PlanError::ClipWithoutClip {
                node: id("shader-clip"),
            },
        ] {
            let error = EvalError::IncompleteComposition {
                clip: ClipId::new("encode-clip").expect("id"),
                composition: CompositionId::new("invalid").expect("id"),
                reason,
            };
            let refused = BindingError::typed(&error);
            assert_eq!(refused.code.as_str(), "INCOMPLETE_COMPOSITION", "{error}");
            assert!(!refused.fatal(), "{error}");
        }
    }

    #[test]
    fn a_refusal_keeps_its_code_whatever_the_ids_it_names() {
        // The HDR refusal stays INVALID_INPUT even when the source ID it names reads like
        // another code (MISSING_INPUT, GPU_DEVICE_LOST).
        for source in ["missing-shot", "device-lost-scene", "shader-intro", "plain"] {
            let refused = BindingError::new(
                Code::INVALID_INPUT,
                format!("the project refuses HDR sources: {source}"),
            );
            assert_eq!(refused.code.as_str(), "INVALID_INPUT");
            assert!(!refused.fatal());
        }
        let image = BindingError::typed(&RenderError::InvalidImageData);
        assert_eq!(image.code, Code::INVALID_INPUT);
        assert!(!image.fatal());
        let dropped = BindingError::typed(&RenderError::GpuWorkDropped);
        assert_eq!(dropped.code, Code::GPU_DEVICE_LOST);
        assert!(dropped.fatal());
        let lost = BindingError::typed(&RenderError::GpuDeviceLost);
        assert_eq!(lost.code, Code::GPU_DEVICE_LOST);
        assert!(lost.fatal());
        // A frame the GPU rejects fails alone; the binding keeps rendering.
        let rejected = BindingError::typed(&RenderError::Gpu {
            kind: alpha2_gpu::GpuErrorKind::Validation,
        });
        assert_eq!(rejected.code, Code::INTERNAL);
        assert!(!rejected.fatal());
    }
}
