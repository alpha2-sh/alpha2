//! The native host's protocol requests: each operation's header parsed into its type and checked
//! before the host opens a device, a decoder or a file.
//!
//! Every request type refuses fields it does not know. A refusal is a [`RequestError`] holding the
//! protocol code and message of the answer.

mod effect;
mod export;
mod extract;
mod media;
mod settings;
mod still;

use std::collections::HashMap;
use std::fmt;

use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;

pub use effect::{
    EffectKindRequest, EffectRequest, InputRequest, ParameterRequest, ParameterTypeRequest,
    ParameterUnitRequest, ResourceKindRequest, ResourceRequest, TierRequest,
};
pub use export::{ExportRequest, ResolvedFormat};
pub use extract::{ExtractHostRequest, PluginRequest};
pub use media::{
    ComposeRequest, ConformRequest, EditRequest, ExportMedia, ProjectJson, ProxyRequest, output_key,
};
pub use settings::{
    AudioQualityRequest, Backlog, DEFAULT_RENDER_CACHE_QUOTA_BYTES, ExportStrategy,
    LoudnessRequest, MAX_QUANTIZER, MemoryRequest, RateRequest, RenderCacheRequest,
    VideoInputPolicy, VideoQualityRequest,
};
pub use still::{MAX_STILL_DIMENSION, MAX_STILLS, StillEntry, StillRequest, StillSize};

#[cfg(test)]
mod tests;

/// The protocol major version this host speaks.
pub const PROTOCOL_VERSION: u32 = 2;

/// The small settings [`parse_request`] reads into a JSON tree for checking (`videoQuality`,
/// `audioQuality`, `renderCache`, `loudness`, `memory`) may be at most this long.
pub const MAX_REQUEST_SETTING_BYTES: usize = 4 * 1024;

/// The longest `cancel` or `status` header the input reader answers at once; a longer one is
/// refused as a request. A cancel names two ids and nothing else, a status one.
pub const MAX_CONTROL_HEADER_BYTES: usize = 64 * 1024;

/// A refused request: the protocol code and the message of the answer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RequestError {
    /// `INVALID_REQUEST`, `INVALID_INPUT`, `UNSUPPORTED_PROTOCOL` or `ALPHA2_ERROR`.
    pub code: &'static str,
    /// Names the field or rule that refused the request.
    pub message: String,
}

impl RequestError {
    /// A refusal with this protocol code.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A malformed request.
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new("INVALID_REQUEST", message)
    }

    /// A well-formed request with a value no rule accepts.
    pub fn invalid_input(message: impl Into<String>) -> Self {
        Self::new("INVALID_INPUT", message)
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for RequestError {}

/// A parsed and checked request header.
pub enum Request {
    /// `capabilities`.
    Capabilities {
        /// Always [`PROTOCOL_VERSION`].
        protocol_version: u32,
        /// The caller's id for the request.
        request_id: String,
    },
    /// `export`.
    Export(Box<ExportRequest>),
    /// `conform`.
    Conform(ConformRequest),
    /// `still`.
    Still(Box<StillRequest>),
    /// `compose`.
    Compose(Box<ComposeRequest>),
    /// `edit`.
    Edit(Box<EditRequest>),
    /// `extract`.
    Extract(Box<ExtractHostRequest>),
}

impl Request {
    /// The caller's id for the request.
    #[must_use]
    pub fn request_id(&self) -> &str {
        match self {
            Self::Capabilities { request_id, .. } => request_id,
            Self::Export(export) => &export.request_id,
            Self::Conform(conform) => &conform.request_id,
            Self::Still(still) => &still.request_id,
            Self::Compose(compose) => &compose.request_id,
            Self::Edit(edit) => &edit.request_id,
            Self::Extract(extract) => &extract.request_id,
        }
    }

    /// The protocol version the request named.
    #[must_use]
    pub fn protocol_version(&self) -> u32 {
        match self {
            Self::Capabilities {
                protocol_version, ..
            } => *protocol_version,
            Self::Export(export) => export.protocol_version,
            Self::Conform(conform) => conform.protocol_version,
            Self::Still(still) => still.protocol_version,
            Self::Compose(compose) => compose.protocol_version,
            Self::Edit(edit) => edit.protocol_version,
            Self::Extract(extract) => extract.protocol_version,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CapabilitiesRequest {
    #[serde(rename = "protocolVersion")]
    protocol_version: u32,
    #[serde(rename = "requestId")]
    request_id: String,
    /// Read by the dispatcher.
    #[serde(default, rename = "operation")]
    _operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
}

/// A `cancel` request.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelRequest {
    #[serde(rename = "protocolVersion")]
    protocol_version: u64,
    /// The cancel's own id.
    #[serde(rename = "requestId", default)]
    pub request_id: String,
    /// The id of the request to cancel.
    #[serde(default)]
    pub target: String,
    /// Read by the dispatcher.
    #[serde(default, rename = "operation")]
    _operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusRequest {
    #[serde(rename = "protocolVersion")]
    protocol_version: u64,
    #[serde(rename = "requestId", default)]
    request_id: String,
    /// Read by the dispatcher.
    #[serde(default, rename = "operation")]
    _operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
}

fn check_version(version: u64) -> Result<(), RequestError> {
    if version == u64::from(PROTOCOL_VERSION) {
        return Ok(());
    }
    Err(RequestError::new(
        "UNSUPPORTED_PROTOCOL",
        format!("unsupported protocol major {version}; this host supports {PROTOCOL_VERSION}"),
    ))
}

#[allow(clippy::needless_pass_by_value)] // Exact signature required by Result::map_err.
fn invalid(error: serde_json::Error) -> RequestError {
    RequestError::invalid_request(format!("invalid request: {error}"))
}

/// The header's top-level fields as raw text. A typed parse alone would also take a JSON array,
/// as the fields in order.
fn fields(line: &str) -> Result<HashMap<String, &RawValue>, RequestError> {
    serde_json::from_str(line).map_err(|error| {
        let what = if error.is_data() {
            "a request is a JSON object"
        } else {
            "invalid JSON"
        };
        RequestError::invalid_request(format!("{what}: {error}"))
    })
}

/// The `requestId` of a `status` request.
///
/// # Errors
/// `UNSUPPORTED_PROTOCOL` for another protocol version, `INVALID_REQUEST` for a malformed header or
/// an empty `requestId`.
pub fn parse_status(line: &str) -> Result<String, RequestError> {
    let status: StatusRequest = serde_json::from_str(line).map_err(invalid)?;
    fields(line)?;
    check_version(status.protocol_version)?;
    if status.request_id.is_empty() {
        return Err(RequestError::invalid_request("requestId must not be empty"));
    }
    Ok(status.request_id)
}

/// # Errors
/// `UNSUPPORTED_PROTOCOL` for another protocol version, `INVALID_REQUEST` for a malformed header or
/// an empty `requestId` or `target`.
pub fn parse_cancel(line: &str) -> Result<CancelRequest, RequestError> {
    let cancel: CancelRequest = serde_json::from_str(line).map_err(invalid)?;
    fields(line)?;
    check_version(cancel.protocol_version)?;
    if cancel.request_id.is_empty() {
        return Err(RequestError::invalid_request("requestId must not be empty"));
    }
    if cancel.target.is_empty() {
        return Err(RequestError::invalid_request(
            "a cancel names the request to cancel in a non-empty `target`",
        ));
    }
    Ok(cancel)
}

/// Parses and checks a request header of any operation but `cancel` and `status`.
///
/// Only the few small settings checked first become JSON trees: the top level is read as raw text
/// per field, the operation's fields parse into their types, and the project stays text until the
/// engine checks its size.
///
/// # Errors
/// `UNSUPPORTED_PROTOCOL` for another protocol version, `INVALID_REQUEST` for a malformed header,
/// an unknown field or operation, or an empty `requestId`, and `INVALID_INPUT` naming a setting or
/// cross-field rule that refuses the request.
pub fn parse_request(line: &str) -> Result<Request, RequestError> {
    let fields = fields(line)?;
    let setting = |name: &str| -> Result<Option<Value>, RequestError> {
        let Some(raw) = fields.get(name) else {
            return Ok(None);
        };
        if raw.get().len() > MAX_REQUEST_SETTING_BYTES {
            return Err(RequestError::invalid_input(format!(
                "{name} is {} bytes; a request setting is at most {MAX_REQUEST_SETTING_BYTES}",
                raw.get().len()
            )));
        }
        serde_json::from_str(raw.get())
            .map(Some)
            .map_err(|error| RequestError::invalid_request(format!("invalid JSON: {error}")))
    };
    let version = fields
        .get("protocolVersion")
        .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
        .ok_or_else(|| {
            RequestError::new(
                "UNSUPPORTED_PROTOCOL",
                "protocolVersion must be the integer 2",
            )
        })?;
    check_version(version)?;
    let operation = fields
        .get("operation")
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok());
    let request = match operation.as_deref() {
        Some("capabilities") => {
            let CapabilitiesRequest {
                protocol_version,
                request_id,
                ..
            } = serde_json::from_str(line).map_err(invalid)?;
            Request::Capabilities {
                protocol_version,
                request_id,
            }
        }
        Some("export") => {
            export::check_settings(&setting)?;
            let export: ExportRequest = serde_json::from_str(line).map_err(invalid)?;
            export.check()?;
            Request::Export(Box::new(export))
        }
        Some("conform") => Request::Conform(serde_json::from_str(line).map_err(invalid)?),
        Some("compose") => Request::Compose(Box::new(serde_json::from_str(line).map_err(invalid)?)),
        Some("edit") => Request::Edit(Box::new(serde_json::from_str(line).map_err(invalid)?)),
        Some("still") => {
            let still: StillRequest = serde_json::from_str(line).map_err(invalid)?;
            still.check()?;
            Request::Still(Box::new(still))
        }
        Some("extract") => {
            let extract: ExtractHostRequest = serde_json::from_str(line).map_err(invalid)?;
            extract.check()?;
            Request::Extract(Box::new(extract))
        }
        // The reader answers every cancel and status it takes; one reaches here only when too long.
        Some(operation @ ("cancel" | "status")) => {
            return Err(RequestError::invalid_request(format!(
                "a {operation} header is at most {MAX_CONTROL_HEADER_BYTES} bytes"
            )));
        }
        Some(other) => {
            return Err(RequestError::invalid_request(format!(
                "invalid request: unknown operation `{other}`, expected one of `capabilities`, `export`, `conform`, `still`, `compose`, `edit`, `extract`, `cancel`, `status`"
            )));
        }
        None => {
            return Err(RequestError::invalid_request(
                "invalid request: missing field `operation`",
            ));
        }
    };
    debug_assert_eq!(request.protocol_version(), PROTOCOL_VERSION);
    if request.request_id().is_empty() {
        return Err(RequestError::invalid_request("requestId must not be empty"));
    }
    Ok(request)
}

/// The header's `requestId`, or `unknown`; the rest of the header is skipped, not parsed into a
/// tree.
#[must_use]
pub fn request_id_from_header(line: &str) -> String {
    #[derive(Deserialize)]
    struct Id {
        #[serde(rename = "requestId")]
        request_id: Option<String>,
    }
    serde_json::from_str::<Id>(line)
        .ok()
        .and_then(|id| id.request_id)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// The header's `operation`; says whether a refusal answers an export, whose error carries a
/// failure report.
#[must_use]
pub fn operation_from_header(line: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct Operation {
        operation: Option<String>,
    }
    serde_json::from_str::<Operation>(line)
        .ok()
        .and_then(|operation| operation.operation)
}
