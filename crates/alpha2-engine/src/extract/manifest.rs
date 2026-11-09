//! A plugin's manifest, naming its identity, artifacts, resources and extractor descriptor.

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use super::Descriptor;

/// The longest manifest text accepted.
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
/// Most artifacts one manifest lists.
pub const MAX_PLUGIN_ARTIFACTS: usize = 64;
/// Most resources one manifest lists.
pub const MAX_PLUGIN_RESOURCES: usize = 64;

/// A plugin's manifest.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    /// Reverse-DNS id, equal to the descriptor's.
    pub id: String,
    /// The package's semantic version.
    pub version: String,
    /// The plugin ABI, `alpha2-plugin/1`.
    #[cfg_attr(feature = "schema", schemars(extend("const" = alpha2_plugin_abi::ABI)))]
    pub abi: String,
    /// The contract the plugin implements.
    pub kind: PluginKind,
    /// The code, one artifact per target.
    #[cfg_attr(feature = "schema", schemars(length(min = 1)))]
    pub artifacts: Vec<PluginArtifact>,
    /// Files the plugin reads through `resource()`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resources: Vec<PluginResource>,
    /// The descriptor `describe()` returns.
    pub descriptor: Descriptor,
}

/// The contract a plugin implements.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// An extractor.
    Extractor,
}

/// One build of a plugin.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PluginArtifact {
    /// `wasm32`, or the Rust target triple a native artifact is built for.
    pub target: String,
    /// What kind of code it is.
    #[serde(rename = "type")]
    pub kind: ArtifactKind,
    /// Where it is, relative to the manifest's own URL or absolute.
    pub url: String,
    /// SHA-256 of its bytes, 64 lowercase hexadecimal digits.
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{64}$")))]
    pub sha256: String,
    /// Its length in bytes.
    pub bytes: u64,
}

/// What kind of code an artifact is.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Hash, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    /// A core wasm module.
    Wasm,
    /// A C-ABI dynamic library.
    Cdylib,
    /// A static library an app links at build time.
    Staticlib,
}

/// A file a plugin reads.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PluginResource {
    /// The name the plugin asks for.
    pub name: String,
    /// Where it is, relative to the manifest's own URL or absolute.
    pub url: String,
    /// SHA-256 of its bytes, 64 lowercase hexadecimal digits.
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{64}$")))]
    pub sha256: String,
    /// Its length in bytes.
    pub bytes: u64,
}

/// Why a manifest was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ManifestError {
    /// The manifest names an ABI this build does not implement.
    UnsupportedAbi(String),
    /// The manifest is not one: bad JSON, a missing or malformed field, or a broken rule.
    Invalid(String),
}

impl fmt::Display for ManifestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedAbi(abi) => write!(
                formatter,
                "the plugin needs ABI {abi}; this build implements {}",
                alpha2_plugin_abi::ABI
            ),
            Self::Invalid(why) => write!(formatter, "invalid plugin manifest: {why}"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl PluginManifest {
    /// Reads and checks a manifest.
    ///
    /// # Errors
    /// [`ManifestError::UnsupportedAbi`] for another ABI, checked before the rest so that a
    /// manifest of a later ABI is reported as such; [`ManifestError::Invalid`] otherwise.
    pub fn from_json(text: &str) -> Result<Self, ManifestError> {
        if text.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::Invalid(format!(
                "longer than {MAX_MANIFEST_BYTES} bytes"
            )));
        }
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|error| ManifestError::Invalid(error.to_string()))?;
        match value.get("abi").and_then(serde_json::Value::as_str) {
            Some(abi) if abi != alpha2_plugin_abi::ABI => {
                return Err(ManifestError::UnsupportedAbi(abi.to_owned()));
            }
            _ => {}
        }
        let manifest: Self = serde_json::from_value(value)
            .map_err(|error| ManifestError::Invalid(error.to_string()))?;
        manifest.validate().map_err(ManifestError::Invalid)?;
        Ok(manifest)
    }

    fn validate(&self) -> Result<(), String> {
        if self.id != self.descriptor.id {
            return Err(format!(
                "id {} differs from the descriptor's {}",
                self.id, self.descriptor.id
            ));
        }
        if !semver(&self.version) {
            return Err(format!(
                "version {} is not a semantic version",
                self.version
            ));
        }
        super::session::validate_descriptor(&self.descriptor).map_err(|error| error.to_string())?;
        if self.artifacts.is_empty() || self.artifacts.len() > MAX_PLUGIN_ARTIFACTS {
            return Err(format!("needs 1 to {MAX_PLUGIN_ARTIFACTS} artifacts"));
        }
        if self.resources.len() > MAX_PLUGIN_RESOURCES {
            return Err(format!("lists more than {MAX_PLUGIN_RESOURCES} resources"));
        }
        let mut targets = HashSet::new();
        for artifact in &self.artifacts {
            if artifact.target.is_empty() || !targets.insert((&artifact.target, artifact.kind)) {
                return Err(format!(
                    "artifact target {:?} is empty or listed twice for one type",
                    artifact.target
                ));
            }
            if (artifact.target == WASM_TARGET) != (artifact.kind == ArtifactKind::Wasm) {
                return Err(format!(
                    "artifact {}: only the wasm32 target is a wasm module",
                    artifact.url
                ));
            }
            check_file(&artifact.url, &artifact.sha256)?;
        }
        let mut names = HashSet::new();
        for resource in &self.resources {
            if resource.name.is_empty() || !names.insert(&resource.name) {
                return Err(format!(
                    "resource name {:?} is empty or repeated",
                    resource.name
                ));
            }
            check_file(&resource.url, &resource.sha256)?;
        }
        Ok(())
    }

    /// The artifact for `target`, by kind.
    #[must_use]
    pub fn artifact(&self, target: &str, kind: ArtifactKind) -> Option<&PluginArtifact> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.target == target && artifact.kind == kind)
    }
}

/// The target of a wasm artifact.
pub const WASM_TARGET: &str = "wasm32";

fn check_file(url: &str, sha256: &str) -> Result<(), String> {
    if url.is_empty() {
        return Err("a URL is empty".to_owned());
    }
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(format!(
            "{url}: sha256 is not 64 lowercase hexadecimal digits"
        ));
    }
    Ok(())
}

/// `MAJOR.MINOR.PATCH` with an optional `-prerelease` and `+build`, as semver 2.0 spells them.
fn semver(version: &str) -> bool {
    let (rest, build) = version
        .split_once('+')
        .map_or((version, None), |(a, b)| (a, Some(b)));
    let (core, pre) = rest
        .split_once('-')
        .map_or((rest, None), |(a, b)| (a, Some(b)));
    let identifiers = |text: &str| {
        text.split('.').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    };
    let numbers = core.split('.').collect::<Vec<_>>();
    numbers.len() == 3
        && numbers.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (part.len() == 1 || !part.starts_with('0'))
        })
        && pre.is_none_or(identifiers)
        && build.is_none_or(identifiers)
}

/// The JSON Schema of a plugin manifest.
#[cfg(feature = "schema")]
#[must_use]
pub fn plugin_manifest_json_schema() -> serde_json::Value {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<PluginManifest>();
    schema.insert(
        "$id".to_owned(),
        serde_json::Value::String(PLUGIN_MANIFEST_SCHEMA_ID.to_owned()),
    );
    schema.insert(
        "title".to_owned(),
        serde_json::Value::String(format!(
            "alpha2 plugin manifest, ABI {}",
            alpha2_plugin_abi::ABI
        )),
    );
    let mut value = schema.to_value();
    alpha2_model::finish_json_schema(&mut value);
    value
}

/// Where the plugin manifest schema is published.
pub const PLUGIN_MANIFEST_SCHEMA_ID: &str = "https://alpha2.sh/schema/plugin-manifest.v1.json";
