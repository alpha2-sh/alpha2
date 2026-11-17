//! The `extract` request.

use std::collections::HashMap;
use std::path::PathBuf;

use alpha2_engine::extract::{Descriptor, ExtractRequest};
use serde::Deserialize;

use super::RequestError;
use super::effect::EffectRequest;
use super::media::{ExportMedia, ProjectJson, ProxyRequest};

/// The `plugin` of an `extract` request, a manifest the host fetches and runs.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRequest {
    /// An `https:` URL, a `file:` URL or a local path.
    pub manifest: String,
    /// SHA-256 of the manifest's bytes.
    pub sha256: String,
    /// Native artifacts run with the host's rights; refused unless allowed.
    #[serde(default)]
    pub allow_native: bool,
    /// The wasm artifact even where a native one is allowed.
    #[serde(default)]
    pub force_wasm: bool,
}

/// An `extract` request.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtractHostRequest {
    /// Always the protocol version.
    pub protocol_version: u32,
    /// The caller's id for the request.
    pub request_id: String,
    /// Always `extract`.
    pub operation: String,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
    /// The project the extraction reads.
    pub project: ProjectJson,
    /// Originals by source ID.
    pub assets: HashMap<String, PathBuf>,
    /// Proxies by source ID.
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    /// Image sequence frames by source ID, in project order; `null` where the sequence has a gap.
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// Files an SVG or a Lottie file names, by source ID and then by locator.
    #[serde(default)]
    pub resources: HashMap<String, HashMap<String, PathBuf>>,
    /// Which media the extraction reads.
    #[serde(default)]
    pub media: ExportMedia,
    /// Lookup tables of runtime effects, by name.
    #[serde(default)]
    pub effect_assets: HashMap<String, PathBuf>,
    /// Font files, in fallback order.
    #[serde(rename = "fonts", default)]
    pub font_files: Vec<PathBuf>,
    /// Runtime effects the project's clips name.
    #[serde(default)]
    pub effects: Vec<EffectRequest>,
    /// The run, as `alpha2_engine::extract::ExtractRequest` writes it.
    pub request: ExtractRequest,
    /// The client-side extractor's descriptor.
    #[serde(default)]
    pub descriptor: Option<Descriptor>,
    /// A plugin the host runs itself.
    #[serde(default)]
    pub plugin: Option<PluginRequest>,
}

impl ExtractHostRequest {
    /// # Errors
    /// `INVALID_REQUEST` for another operation, or unless exactly one of `descriptor` and `plugin`
    /// is given.
    pub fn check(&self) -> Result<(), RequestError> {
        if self.operation != "extract" {
            return Err(RequestError::invalid_request(
                "invalid request: the operation is not `extract`",
            ));
        }
        if self.descriptor.is_some() == self.plugin.is_some() {
            return Err(RequestError::invalid_request(
                "an extract request has exactly one of `descriptor` and `plugin`",
            ));
        }
        Ok(())
    }
}
