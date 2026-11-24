//! Runs plugins an app bundles, a manifest and its files inside the app, checked against their
//! pinned hashes. App stores forbid downloading code, so nothing is fetched and every URL in the
//! manifest must be relative to it.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_engine::extract::{Reference, reference};
use alpha2_host_core::plugins::{
    Fetch, Fetched, LoadRequest, PluginError, PluginExtractor, Policy, ResourceRoot, codes, load,
    parse_manifest, sha256_hex,
};

use crate::error::Alpha2Error;
use crate::extract::{ExtractRequest, ExtractionHandle, Run, RunExtractor};
use crate::session::Alpha2;

fn plugin_error(error: PluginError) -> Alpha2Error {
    if error.code == alpha2_host_core::plugins::CANCELLED {
        return Alpha2Error::Aborted;
    }
    Alpha2Error::Failed {
        details: error
            .details
            .map(|details| serde_json::Value::Object(details).to_string()),
        code: error.code.into(),
        reason: error.message,
    }
}

impl Run for PluginExtractor {
    fn made_by(&self) -> Option<alpha2_model::MadeByPlugin> {
        Some(PluginExtractor::made_by(self))
    }
}

/// Files beside the manifest.
struct Bundled(PathBuf);

impl Fetch for Bundled {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<Fetched, PluginError> {
        match reference(url) {
            Ok(Reference::Relative(path)) => Ok(Fetched {
                path: self.0.join(path),
                local: true,
            }),
            _ => Err(PluginError::new(
                codes::FETCH_FAILED,
                format!(
                    "{url}: apps bundle their plugins, so only paths under the manifest are read"
                ),
            )),
        }
    }
}

/// A bundled plugin, checked and ready to run.
#[derive(uniffi::Object)]
pub struct Plugin {
    manifest: alpha2_engine::extract::PluginManifest,
    manifest_sha256: String,
    directory: PathBuf,
    supplied: BTreeMap<String, PathBuf>,
    policy: Policy,
}

#[uniffi::export]
impl Plugin {
    /// The plugin whose manifest is the file at `manifest_path`, which has SHA-256 `sha256`.
    /// Its native artifact runs only when `allow_native`, in the app's process; otherwise its
    /// sandboxed wasm one does. `supplied` maps each resource slot the app fills to the absolute
    /// path of a file or directory the app may read; they are checked when the plugin runs.
    ///
    /// # Errors
    /// `PLUGIN_FETCH_FAILED` for an unreadable manifest, `PLUGIN_HASH_MISMATCH`,
    /// `PLUGIN_ABI_UNSUPPORTED` or `PLUGIN_LOAD_FAILED` for a manifest that is not one.
    #[uniffi::constructor]
    pub fn load(
        manifest_path: String,
        sha256: String,
        allow_native: bool,
        supplied: HashMap<String, String>,
    ) -> Result<Arc<Self>, Alpha2Error> {
        let path = PathBuf::from(&manifest_path);
        let bytes = std::fs::read(&path).map_err(|error| {
            plugin_error(PluginError::new(
                codes::FETCH_FAILED,
                format!("{manifest_path}: {error}"),
            ))
        })?;
        let found = sha256_hex(&bytes);
        if found != sha256.to_ascii_lowercase() {
            return Err(plugin_error(PluginError::new(
                codes::HASH_MISMATCH,
                format!("the manifest has SHA-256 {found}; the app pins {sha256}"),
            )));
        }
        let text = String::from_utf8(bytes).map_err(|_| {
            plugin_error(PluginError::new(
                codes::LOAD_FAILED,
                "the manifest is not UTF-8",
            ))
        })?;
        let manifest = parse_manifest(&text).map_err(plugin_error)?;
        Ok(Arc::new(Self {
            manifest,
            manifest_sha256: found,
            directory: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            supplied: supplied
                .into_iter()
                .map(|(slot, path)| (slot, PathBuf::from(path)))
                .collect(),
            policy: Policy {
                allow_native,
                resource_root: ResourceRoot::Any,
                ..Policy::default()
            },
        }))
    }

    /// The plugin's id, which its extractor's data records.
    #[must_use]
    pub fn id(&self) -> String {
        self.manifest.id.clone()
    }

    /// The extractor's descriptor, as JSON.
    #[must_use]
    pub fn descriptor_json(&self) -> String {
        serde_json::to_string(&self.manifest.descriptor).unwrap_or_default()
    }

    /// What the manifest's `about` says for an interface, as JSON, or `None` without one.
    #[must_use]
    pub fn about_json(&self) -> Option<String> {
        self.manifest
            .about
            .as_ref()
            .and_then(|about| serde_json::to_string(about).ok())
    }
}

#[uniffi::export]
impl Alpha2 {
    /// Starts `plugin` over `request` on a library thread; the request's `params` go to it.
    ///
    /// # Errors
    /// [`Alpha2Error::InvalidInput`] for an invalid project or request; everything later, the
    /// `PLUGIN_*` codes included, is the handle's [`ExtractionHandle::outcome`] result.
    pub fn extract_plugin(
        self: Arc<Self>,
        request: ExtractRequest,
        plugin: Arc<Plugin>,
    ) -> Result<Arc<ExtractionHandle>, Alpha2Error> {
        let run: alpha2_engine::extract::ExtractRequest =
            serde_json::from_str(&request.request_json)
                .map_err(|error| Alpha2Error::invalid(format!("invalid request: {error}")))?;
        self.start_extraction(
            request,
            Box::new(move |cancel| {
                let mut bundled = Bundled(plugin.directory.clone());
                let extractor = load(
                    LoadRequest {
                        manifest: &plugin.manifest,
                        manifest_sha256: &plugin.manifest_sha256,
                        params: &run.params,
                        supplied: &plugin.supplied,
                    },
                    &plugin.policy,
                    &mut bundled,
                    cancel,
                )
                .map_err(plugin_error)?;
                Ok(RunExtractor {
                    extractor: Box::new(extractor),
                })
            }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loaded_plugin_gives_its_about_and_output_labels_for_interfaces() {
        let corpus: Vec<serde_json::Value> = serde_json::from_str(include_str!(
            "../../../tests/cases/plugin-manifest-corpus.json"
        ))
        .expect("corpus");
        let dir = std::env::temp_dir().join(format!("alpha2-mobile-about-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let load = |name: &str| {
            let case = corpus
                .iter()
                .find(|case| case["name"] == name)
                .expect("case");
            let text = case["manifest"].to_string();
            let path = dir.join(format!("{name}.json"));
            std::fs::write(&path, &text).expect("manifest");
            Plugin::load(
                path.to_string_lossy().into_owned(),
                sha256_hex(text.as_bytes()),
                false,
                HashMap::new(),
            )
            .expect("loads")
        };
        let labelled = load("about-and-output-labels");
        let plain = load("complete");
        let _ = std::fs::remove_dir_all(&dir);
        let about: serde_json::Value =
            serde_json::from_str(&labelled.about_json().expect("about")).expect("json");
        assert_eq!(about["name"], "Silence");
        let descriptor: serde_json::Value =
            serde_json::from_str(&labelled.descriptor_json()).expect("json");
        assert_eq!(descriptor["outputs"][0]["label"], "Silences");
        assert_eq!(plain.about_json(), None);
    }

    #[test]
    fn a_bundled_plugin_reads_only_paths_under_its_manifest() {
        let mut bundled = Bundled(PathBuf::from("/app/plugin"));
        assert_eq!(
            bundled.fetch("x86_64/a.so", "", 0),
            Ok(Fetched {
                path: PathBuf::from("/app/plugin/x86_64/a.so"),
                local: true,
            })
        );
        for refused in [
            "../x",
            "a/../../x",
            "/etc/hostname",
            "file:///etc/hostname",
            "https://example.com/a.wasm",
        ] {
            assert_eq!(
                bundled.fetch(refused, "", 0).expect_err(refused).code,
                codes::FETCH_FAILED
            );
        }
    }
}
