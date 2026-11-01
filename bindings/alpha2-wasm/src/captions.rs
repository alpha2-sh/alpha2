//! Project authoring helpers for the web, shared with native: subtitle import, caption font
//! families, and project lint.

use alpha2_model::{ImportedCaptions, Project, caption_rows, parse_srt, parse_webvtt};
use wasm_bindgen::prelude::*;

use crate::web::{internal_error, invalid_input, typed_error};

/// Reads a subtitle file (`format` is `srt` or `webvtt`) into caption cues, returned as JSON
/// `{ rows, skipped, notes }`.
///
/// `rows` holds one caption track's `CaptionCue`s per row, split so no cues overlap within a
/// row and no row exceeds a caption track's cue limit. `skipped` counts blocks that were not
/// cues; `notes` counts markup read but not kept (`extraVoices`, `droppedTimestamps`,
/// `droppedClasses`, `flattened`).
///
/// # Errors
/// Rejects an unknown format, a WebVTT file without its header, and cues that need more
/// caption tracks than a project holds.
#[wasm_bindgen(js_name = parseCaptions)]
pub fn parse_captions(text: &str, format: &str) -> Result<String, JsValue> {
    let imported: ImportedCaptions = match format {
        "srt" => parse_srt(text).map_err(invalid_input)?,
        "webvtt" => parse_webvtt(text).map_err(invalid_input)?,
        other => {
            return Err(invalid_input(format!(
                "caption format must be srt or webvtt, not {other}"
            )));
        }
    };
    let rows = caption_rows(&imported.cues).map_err(invalid_input)?;
    let notes = imported.notes;
    serde_json::to_string(&serde_json::json!({
        "rows": rows,
        "skipped": imported.skipped,
        "notes": {
            "extraVoices": notes.extra_voices,
            "droppedTimestamps": notes.dropped_timestamps,
            "droppedClasses": notes.dropped_classes,
            "flattened": notes.flattened,
        },
    }))
    .map_err(internal_error)
}

/// Returns the family name a caption style's `font_family` must use to select face
/// `face_index` of the font `bytes`, or `undefined` when the bytes are not a font or the face
/// names no family.
#[wasm_bindgen(js_name = fontFamily)]
#[must_use]
pub fn font_family(bytes: &[u8], face_index: u32) -> Option<String> {
    alpha2_text::font_family(bytes, face_index)
}

/// Checks that `bytes` hold face `face_index` of a font the engine can draw with, as
/// registering it would.
///
/// # Errors
/// Rejects bytes the font parser refuses, with its reason (for example a WOFF container named
/// `.ttf`).
#[wasm_bindgen(js_name = checkFont)]
pub fn check_font(bytes: &[u8], face_index: u32) -> Result<(), JsValue> {
    alpha2_text::check_font(bytes, face_index).map_err(invalid_input)
}

/// A project's non-fatal diagnostics as a JSON array of `LintIssue`s.
///
/// # Errors
/// Rejects a document that does not load as a valid project.
#[wasm_bindgen(js_name = lintProject)]
pub fn lint_project(project_json: &str) -> Result<String, JsValue> {
    let project = Project::from_json(project_json).map_err(|error| typed_error(&error))?;
    serde_json::to_string(&project.lint()).map_err(internal_error)
}
