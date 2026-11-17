//! The project and media fields several operations share, and the operations that take only those.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use alpha2_assets::MediaPurpose;
use alpha2_model::MediaFingerprint;
use serde::Deserialize;
use serde_json::value::RawValue;
use unicode_normalization::UnicodeNormalization;

/// A request's project, kept as text so the engine checks its size before anything parses it.
#[derive(Deserialize)]
#[serde(transparent)]
pub struct ProjectJson(Box<RawValue>);

impl ProjectJson {
    /// The project of a request the host builds itself.
    ///
    /// # Errors
    /// When `project` does not serialize.
    pub fn from_value(project: &serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::value::to_raw_value(project).map(Self)
    }

    /// The project's JSON text, as the request held it.
    #[must_use]
    pub fn text(&self) -> &str {
        self.0.get()
    }
}

/// A proxy of a source, as the request names it.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProxyRequest {
    /// The proxy file.
    pub path: PathBuf,
    /// The original the proxy was made from; a proxy of another original is stale.
    #[serde(default)]
    pub made_from: Option<MediaFingerprint>,
    /// Set when the proxy was tone-mapped.
    #[serde(default)]
    pub color_pipeline: Option<String>,
}

impl ProxyRequest {
    /// A proxy at `path` with nothing known about how it was made.
    #[must_use]
    pub const fn at(path: PathBuf) -> Self {
        Self {
            path,
            made_from: None,
            color_pipeline: None,
        }
    }
}

/// Which media an export or still reads.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum ExportMedia {
    /// The originals.
    #[default]
    Original,
    /// Fresh proxies, and originals where there are none.
    Proxies,
}

impl ExportMedia {
    /// The purpose the media is selected for.
    #[must_use]
    pub const fn purpose(self) -> MediaPurpose {
        match self {
            Self::Original => MediaPurpose::ExportOriginal,
            Self::Proxies => MediaPurpose::ExportProxy,
        }
    }
}

/// A `conform` request.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConformRequest {
    /// Always the protocol version.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: u32,
    /// The caller's id for the request.
    #[serde(rename = "requestId")]
    pub request_id: String,
    /// Read by the dispatcher.
    #[serde(default, rename = "operation")]
    _operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
    /// The project to conform.
    pub project: ProjectJson,
    /// Originals by source ID.
    #[serde(default)]
    pub assets: HashMap<String, PathBuf>,
    /// Proxies by source ID.
    #[serde(default)]
    pub proxies: HashMap<String, Vec<ProxyRequest>>,
    /// Image sequence frames by source ID, in project order; `null` where the sequence has a gap.
    #[serde(default)]
    pub frames: HashMap<String, Vec<Option<PathBuf>>>,
    /// What the media is selected for; the export's originals when absent.
    #[serde(default = "export_original")]
    pub purpose: MediaPurpose,
}

const fn export_original() -> MediaPurpose {
    MediaPurpose::ExportOriginal
}

/// A compose or template request, answered without a GPU or reading media.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ComposeRequest {
    /// Always the protocol version.
    pub protocol_version: u32,
    /// The caller's id for the request.
    pub request_id: String,
    /// Read by the dispatcher.
    #[serde(default, rename = "operation")]
    _operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
    /// `compile`, `bind`, `dataSchema`, `lint`, `requirements` or `templates`.
    pub action: String,
    /// The spec `compile` and `requirements` read.
    #[serde(default)]
    pub spec: Option<serde_json::Value>,
    /// The template `bind`, `dataSchema` and `lint` read.
    #[serde(default)]
    pub template: Option<serde_json::Value>,
    /// The data a template is bound to.
    #[serde(default)]
    pub data: Option<serde_json::Value>,
    /// Facts about the spec's media.
    #[serde(default)]
    pub facts: Option<serde_json::Value>,
    /// Font files `compile` measures text with.
    #[serde(default)]
    pub fonts: Vec<PathBuf>,
    /// The theme a spec is compiled with.
    #[serde(default)]
    pub theme: Option<serde_json::Value>,
    /// Templates `templates` lists.
    #[serde(default)]
    pub templates: Vec<serde_json::Value>,
}

/// An edit operation applied to a project, answered without a GPU or reading media.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EditRequest {
    /// Always the protocol version.
    pub protocol_version: u32,
    /// The caller's id for the request.
    pub request_id: String,
    /// Read by the dispatcher.
    #[serde(default, rename = "operation")]
    _operation: serde::de::IgnoredAny,
    /// The frame's attachment list, read with the frame.
    #[serde(default, rename = "attachments")]
    _attachments: serde::de::IgnoredAny,
    /// The project to edit.
    pub project: ProjectJson,
    /// The operation: `remove_ranges`, `split_at` or `bake`.
    pub edit: Box<RawValue>,
}

/// The absolute path, NFC-composed and lower-cased. Windows and macOS file systems usually ignore
/// case, and macOS also normalization; the clients compare by the same rule.
#[must_use]
pub fn output_key(path: &Path) -> String {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    absolute
        .to_string_lossy()
        .nfc()
        .collect::<String>()
        .to_lowercase()
}
