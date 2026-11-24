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
/// The longest artifact a manifest may pin, in bytes.
pub const MAX_PLUGIN_ARTIFACT_BYTES: u64 = 256 * 1024 * 1024;
/// The longest resource a manifest may pin, in bytes: what a wasm module's memory can address.
pub const MAX_PLUGIN_RESOURCE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// The most bytes a manifest's resources may pin together.
pub const MAX_PLUGIN_RESOURCES_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;

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
    /// Where it is: a path under the manifest's own directory, or an absolute `https:` URL.
    pub url: String,
    /// SHA-256 of its bytes, 64 lowercase hexadecimal digits.
    #[cfg_attr(feature = "schema", schemars(regex(pattern = r"^[0-9a-f]{64}$")))]
    pub sha256: String,
    /// Its length in bytes.
    #[cfg_attr(feature = "schema", schemars(range(max = MAX_PLUGIN_ARTIFACT_BYTES)))]
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

/// The most resource slots one manifest declares, inside [`MAX_PLUGIN_RESOURCES`].
pub const MAX_PLUGIN_SLOTS: usize = 16;
/// The most file name suffixes one slot's `accept` lists.
pub const MAX_SLOT_ACCEPT: usize = 16;
/// The longest file name suffix in a slot's `accept`, in bytes.
pub const MAX_SLOT_ACCEPT_BYTES: usize = 64;
/// The longest slot name, in bytes.
pub const MAX_SLOT_NAME_BYTES: usize = 64;

/// A file a plugin reads: pinned by the manifest, or supplied by the caller at load time.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(try_from = "ResourceEntry", into = "ResourceEntry")]
pub struct PluginResource {
    /// The name the plugin asks for.
    pub name: String,
    /// Where its bytes come from.
    pub source: ResourceSource,
}

/// Where a resource's bytes come from.
#[derive(Clone, Debug, PartialEq)]
pub enum ResourceSource {
    /// A file the manifest pins by URL, SHA-256 and length.
    Pinned(PinnedResource),
    /// A file or directory the caller names when it loads the plugin.
    Supplied(SuppliedSlot),
}

/// A file the manifest pins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedResource {
    /// Where it is: a path under the manifest's own directory, or an absolute `https:` URL.
    pub url: String,
    /// SHA-256 of its bytes, 64 lowercase hexadecimal digits.
    pub sha256: String,
    /// Its length in bytes.
    pub bytes: u64,
}

/// A resource slot the caller fills at load time.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct SuppliedSlot {
    /// What the caller names.
    pub shape: SlotShape,
    /// Whether a load without it fails. An optional slot left empty reads as an undeclared name.
    #[serde(default = "required_default", skip_serializing_if = "is_true")]
    #[cfg_attr(feature = "schema", schemars(extend("default" = true)))]
    pub required: bool,
    /// The largest single file the plugin takes from the slot, in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "schema", schemars(range(min = 1)))]
    pub max_bytes: Option<u64>,
    /// File name suffixes a picker offers, such as `.onnx`; for interfaces only.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(
        feature = "schema",
        schemars(length(max = MAX_SLOT_ACCEPT), inner(length(min = 1, max = MAX_SLOT_ACCEPT_BYTES)))
    )]
    pub accept: Vec<String>,
}

const fn required_default() -> bool {
    true
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde passes a reference"
)]
const fn is_true(value: &bool) -> bool {
    *value
}

/// What the caller names for a slot.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum SlotShape {
    /// One file, read as the resource named as the slot.
    File,
    /// A directory; each file under it is the resource `<slot>/<relative path>`.
    Directory,
}

impl PluginResource {
    /// The pin, for a pinned resource.
    #[must_use]
    pub const fn pinned(&self) -> Option<&PinnedResource> {
        match &self.source {
            ResourceSource::Pinned(pinned) => Some(pinned),
            ResourceSource::Supplied(_) => None,
        }
    }

    /// The slot, for a supplied resource.
    #[must_use]
    pub const fn slot(&self) -> Option<&SuppliedSlot> {
        match &self.source {
            ResourceSource::Pinned(_) => None,
            ResourceSource::Supplied(slot) => Some(slot),
        }
    }
}

/// A resource as JSON spells it: the pin's fields, or `supplied`.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ResourceEntry {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    supplied: Option<SuppliedSlot>,
}

impl TryFrom<ResourceEntry> for PluginResource {
    type Error = String;

    fn try_from(entry: ResourceEntry) -> Result<Self, String> {
        let ResourceEntry {
            name,
            url,
            sha256,
            bytes,
            supplied,
        } = entry;
        let source = match (url, sha256, bytes, supplied) {
            (Some(url), Some(sha256), Some(bytes), None) => {
                ResourceSource::Pinned(PinnedResource { url, sha256, bytes })
            }
            (None, None, None, Some(slot)) => ResourceSource::Supplied(slot),
            (None, None, None, None) => {
                return Err(format!(
                    "resource {name:?} has neither url, sha256 and bytes nor supplied"
                ));
            }
            (_, _, _, Some(_)) => {
                return Err(format!(
                    "resource {name:?} has supplied and a pin; a resource is one or the other"
                ));
            }
            _ => {
                return Err(format!(
                    "resource {name:?} needs all of url, sha256 and bytes"
                ));
            }
        };
        Ok(Self { name, source })
    }
}

impl From<PluginResource> for ResourceEntry {
    fn from(resource: PluginResource) -> Self {
        let mut entry = Self {
            name: resource.name,
            url: None,
            sha256: None,
            bytes: None,
            supplied: None,
        };
        match resource.source {
            ResourceSource::Pinned(PinnedResource { url, sha256, bytes }) => {
                entry.url = Some(url);
                entry.sha256 = Some(sha256);
                entry.bytes = Some(bytes);
            }
            ResourceSource::Supplied(slot) => entry.supplied = Some(slot),
        }
        entry
    }
}

#[cfg(feature = "schema")]
mod resource_schema {
    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};

    use super::{MAX_PLUGIN_RESOURCE_BYTES, SuppliedSlot};

    /// A file the manifest pins by URL, SHA-256 and length.
    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields, rename = "PinnedResource")]
    #[expect(dead_code, reason = "a shape for the schema alone")]
    struct PinnedResourceEntry {
        /// The name the plugin asks for.
        #[schemars(length(min = 1))]
        name: String,
        /// Where it is: a path under the manifest's own directory, or an absolute `https:` URL.
        url: String,
        /// SHA-256 of its bytes, 64 lowercase hexadecimal digits.
        #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
        sha256: String,
        /// Its length in bytes.
        #[schemars(range(max = MAX_PLUGIN_RESOURCE_BYTES))]
        bytes: u64,
    }

    /// A slot the caller fills with a file or a directory when it loads the plugin.
    #[derive(JsonSchema)]
    #[schemars(deny_unknown_fields, rename = "SuppliedResource")]
    #[expect(dead_code, reason = "a shape for the schema alone")]
    struct SuppliedResourceEntry {
        /// The slot's name, which the plugin asks for.
        #[schemars(regex(pattern = r"^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$"))]
        name: String,
        /// What the caller fills it with.
        supplied: SuppliedSlot,
    }

    impl JsonSchema for super::PluginResource {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            "PluginResource".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "description": "A file a plugin reads: pinned by the manifest, or a slot the \
                                caller fills at load time.",
                "oneOf": [
                    generator.subschema_for::<PinnedResourceEntry>(),
                    generator.subschema_for::<SuppliedResourceEntry>(),
                ]
            })
        }
    }
}

/// Whether `name` is a slot name: an ASCII letter, digit, `_` or `-`, then up to 63 of those or
/// `.`.
#[must_use]
pub fn is_slot_name(name: &str) -> bool {
    let allowed = |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-');
    name.len() <= MAX_SLOT_NAME_BYTES
        && name.bytes().next().is_some_and(allowed)
        && name.bytes().all(|byte| allowed(byte) || byte == b'.')
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
            if artifact.bytes > MAX_PLUGIN_ARTIFACT_BYTES {
                return Err(format!(
                    "artifact {} is longer than {MAX_PLUGIN_ARTIFACT_BYTES} bytes",
                    artifact.url
                ));
            }
        }
        self.validate_resources()?;
        Ok(())
    }

    fn validate_resources(&self) -> Result<(), String> {
        let mut names = HashSet::new();
        let mut directories = Vec::new();
        let (mut slots, mut total) = (0_usize, 0_u64);
        for resource in &self.resources {
            if resource.name.is_empty() || !names.insert(&resource.name) {
                return Err(format!(
                    "resource name {:?} is empty or repeated",
                    resource.name
                ));
            }
            match &resource.source {
                ResourceSource::Pinned(pinned) => {
                    check_file(&pinned.url, &pinned.sha256)?;
                    if pinned.bytes > MAX_PLUGIN_RESOURCE_BYTES {
                        return Err(format!(
                            "resource {} is longer than {MAX_PLUGIN_RESOURCE_BYTES} bytes",
                            resource.name
                        ));
                    }
                    total = total.saturating_add(pinned.bytes);
                }
                ResourceSource::Supplied(slot) => {
                    slots += 1;
                    check_slot(&resource.name, slot)?;
                    if slot.shape == SlotShape::Directory {
                        directories.push(format!("{}/", resource.name));
                    }
                }
            }
        }
        if slots > MAX_PLUGIN_SLOTS {
            return Err(format!(
                "declares more than {MAX_PLUGIN_SLOTS} resource slots"
            ));
        }
        if let Some(resource) = self.resources.iter().find(|resource| {
            resource.pinned().is_some()
                && directories
                    .iter()
                    .any(|prefix| resource.name.starts_with(prefix.as_str()))
        }) {
            return Err(format!(
                "pinned resource {} lies under a directory slot's names",
                resource.name
            ));
        }
        if total > MAX_PLUGIN_RESOURCES_TOTAL_BYTES {
            return Err(format!(
                "the pinned resources are longer than {MAX_PLUGIN_RESOURCES_TOTAL_BYTES} bytes \
                 together"
            ));
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

/// What a manifest's `url` names.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Reference<'a> {
    /// A path under the manifest's own directory, `/`-separated.
    Relative(&'a str),
    /// An absolute `https:` URL.
    Https(&'a str),
}

/// The scheme of `url`, when it starts with one (RFC 3986).
fn scheme(url: &str) -> Option<&str> {
    let (scheme, _) = url.split_once(':')?;
    let valid = scheme.starts_with(|c: char| c.is_ascii_alphabetic())
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(scheme)
}

/// Reads a manifest's `url` as a path under the manifest's directory or an `https:` URL.
///
/// # Errors
/// For any other scheme, an absolute or drive-prefixed path, a backslash or NUL, and a path whose
/// `..` segments climb above the manifest's directory.
pub fn reference(url: &str) -> Result<Reference<'_>, String> {
    let refused = |why: &str| Err(format!("{url}: {why}"));
    if url.is_empty() {
        return Err("a URL is empty".to_owned());
    }
    match scheme(url) {
        Some("https") => return Ok(Reference::Https(url)),
        Some(scheme) if scheme.len() == 1 => {
            return refused("a drive-prefixed path is not under the manifest");
        }
        Some(_) => return refused("only https: URLs and paths under the manifest are allowed"),
        None => {}
    }
    if url.contains(['\\', '\0']) {
        return refused("a path takes neither backslashes nor NUL");
    }
    if url.starts_with('/') {
        return refused("an absolute path is not under the manifest");
    }
    let path = url.split(['?', '#']).next().unwrap_or_default();
    let mut depth = 0_usize;
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| format!("{url}: the path climbs above the manifest"))?;
            }
            _ => depth += 1,
        }
    }
    Ok(Reference::Relative(url))
}

fn check_slot(name: &str, slot: &SuppliedSlot) -> Result<(), String> {
    if !is_slot_name(name) {
        return Err(format!(
            "slot name {name:?} is not 1 to {MAX_SLOT_NAME_BYTES} of A-Z, a-z, 0-9, '.', '_' and \
             '-', not starting with '.'"
        ));
    }
    if slot.max_bytes == Some(0) {
        return Err(format!("slot {name}: max_bytes is 0"));
    }
    if slot.accept.len() > MAX_SLOT_ACCEPT {
        return Err(format!(
            "slot {name}: accept lists more than {MAX_SLOT_ACCEPT} suffixes"
        ));
    }
    if let Some(suffix) = slot
        .accept
        .iter()
        .find(|suffix| suffix.is_empty() || suffix.len() > MAX_SLOT_ACCEPT_BYTES)
    {
        return Err(format!(
            "slot {name}: accept suffix {suffix:?} is empty or longer than \
             {MAX_SLOT_ACCEPT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn check_file(url: &str, sha256: &str) -> Result<(), String> {
    reference(url)?;
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
