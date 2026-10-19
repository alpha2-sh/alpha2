//! The engine's asset decisions for the web, made by the same code as
//! natively (`alpha2-assets`): whether found media is the recorded media,
//! and whether a project's media is ready for a purpose.

use alpha2_assets::{AssetCatalog, MediaPurpose, color_pipeline_fingerprint, compare, conform};
use alpha2_model::{ColorPipeline, MediaFingerprint, Project};
use wasm_bindgen::prelude::*;

use crate::web::{internal_error, invalid_input, typed_error};

/// Compares a found fingerprint with a recorded one (both JSON
/// `MediaFingerprint`s), as JSON `FingerprintMatch`: `identical`,
/// `consistent`, `mismatch` with its fields, or `unverified`.
///
/// # Errors
/// Rejects JSON that is not a fingerprint, or a fingerprint with a part that
/// is not well formed (`MediaFingerprint::malformed_part`), such as a hash
/// in uppercase: that is an input error, not other media.
#[wasm_bindgen(js_name = compareFingerprints)]
pub fn compare_fingerprints(recorded: &str, found: &str) -> Result<String, JsValue> {
    let recorded: MediaFingerprint = serde_json::from_str(recorded).map_err(invalid_input)?;
    let found: MediaFingerprint = serde_json::from_str(found).map_err(invalid_input)?;
    for (which, fingerprint) in [("recorded", &recorded), ("found", &found)] {
        if let Some(part) = fingerprint.malformed_part() {
            return Err(invalid_input(format!(
                "the {which} fingerprint has a malformed {part}"
            )));
        }
    }
    serde_json::to_string(&compare(&recorded, &found)).map_err(internal_error)
}

/// Whether `project`'s media, as `catalog` describes it, is ready for
/// `purpose` (`preview`, `thumbnail`, `remote_edit`, `export_original`, or
/// `export_proxy`), as a JSON `ConformReport`.
///
/// # Errors
/// Rejects an invalid project, catalog (one whose fingerprints are not well
/// formed among them), or purpose.
#[wasm_bindgen(js_name = conformProject)]
pub fn conform_project(project: &str, catalog: &str, purpose: &str) -> Result<String, JsValue> {
    let project = Project::from_json(project).map_err(|error| typed_error(&error))?;
    let catalog: AssetCatalog = serde_json::from_str(catalog).map_err(invalid_input)?;
    catalog.validate().map_err(invalid_input)?;
    let purpose = match purpose {
        "preview" => MediaPurpose::Preview,
        "thumbnail" => MediaPurpose::Thumbnail,
        "remote_edit" => MediaPurpose::RemoteEdit,
        "export_original" => MediaPurpose::ExportOriginal,
        "export_proxy" => MediaPurpose::ExportProxy,
        other => return Err(invalid_input(format!("unknown media purpose {other}"))),
    };
    serde_json::to_string(&conform(&project, &catalog, purpose)).map_err(internal_error)
}

/// Returns a color pipeline's identity (JSON `ColorPipeline`), in the form a
/// proxy that bakes the pipeline in records it.
///
/// # Errors
/// Rejects JSON that is not a color pipeline.
#[wasm_bindgen(js_name = colorPipelineFingerprint)]
pub fn color_pipeline_fingerprint_js(pipeline: &str) -> Result<String, JsValue> {
    let pipeline: ColorPipeline = serde_json::from_str(pipeline).map_err(invalid_input)?;
    Ok(color_pipeline_fingerprint(&pipeline))
}
