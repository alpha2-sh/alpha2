//! Runs plugins an app bundles, a manifest and its files inside the app, checked against their
//! pinned hashes. App stores forbid downloading code, so nothing is fetched and every URL in the
//! manifest must be relative to it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use alpha2_host_core::plugins::{
    Fetch, PluginError, Policy, codes, load, parse_manifest, sha256_hex,
};

use crate::error::Alpha2Error;
use crate::extract::{ExtractRequest, ExtractionHandle, RunExtractor};
use crate::session::Alpha2;

fn plugin_error(error: PluginError) -> Alpha2Error {
    Alpha2Error::Failed {
        code: error.code.into(),
        reason: error.message,
    }
}

/// Files beside the manifest.
struct Bundled(PathBuf);

impl Fetch for Bundled {
    fn fetch(&mut self, url: &str, _sha256: &str, _bytes: u64) -> Result<PathBuf, PluginError> {
        if url.contains("://") || Path::new(url).is_absolute() {
            return Err(PluginError::new(
                codes::FETCH_FAILED,
                format!(
                    "{url}: apps bundle their plugins, so only paths relative to the manifest are read"
                ),
            ));
        }
        Ok(self.0.join(url))
    }
}

/// A bundled plugin, checked and ready to run.
#[derive(uniffi::Object)]
pub struct Plugin {
    manifest: alpha2_engine::extract::PluginManifest,
    directory: PathBuf,
    policy: Policy,
}

#[uniffi::export]
impl Plugin {
    /// The plugin whose manifest is the file at `manifest_path`, which has SHA-256 `sha256`.
    /// Its native artifact runs only when `allow_native`; otherwise its sandboxed wasm one does.
    ///
    /// # Errors
    /// `PLUGIN_FETCH_FAILED` for an unreadable manifest, `PLUGIN_HASH_MISMATCH`,
    /// `PLUGIN_ABI_UNSUPPORTED` or `PLUGIN_LOAD_FAILED` for a manifest that is not one.
    #[uniffi::constructor]
    pub fn load(
        manifest_path: String,
        sha256: String,
        allow_native: bool,
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
            directory: path.parent().map(Path::to_path_buf).unwrap_or_default(),
            policy: Policy {
                allow_native,
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
            Box::new(move || {
                let mut bundled = Bundled(plugin.directory.clone());
                let extractor = load(&plugin.manifest, run.params, &plugin.policy, &mut bundled)
                    .map_err(plugin_error)?;
                let made_by = extractor.made_by().clone();
                Ok(RunExtractor {
                    extractor: Box::new(extractor),
                    plugin: Some(made_by),
                })
            }),
        )
    }
}
