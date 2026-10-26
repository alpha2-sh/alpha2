//! `alpha2._native`, the compiled part of the `alpha2` Python package.
//!
//! It gives Python the engine's own answers where a second implementation would drift: whether a
//! project document is valid (`alpha2_model`, the validator the host runs), where a project ends,
//! and which container and codecs an export resolves to (`alpha2_media`). It links neither a GPU
//! nor `FFmpeg`; rendering is the `alpha2-host` process's.
//!
//! Documents cross the boundary as JSON text, which the Python side writes once with the same
//! rules it sends the host.

use std::str::FromStr;

use alpha2_media::{AudioCodec, Container, VideoCodec};
use alpha2_model::{CURRENT_SCHEMA_VERSION, MAX_PROJECT_JSON_BYTES, Project};
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

/// The problems the engine finds in a project document, as JSON text: empty when it is valid. The
/// engine stops at its first problem, so the list holds at most one.
#[pyfunction]
fn validate_project_json(py: Python<'_>, json: &str) -> Vec<String> {
    py.detach(|| match Project::from_json(json) {
        Ok(_) => Vec::new(),
        Err(error) => vec![error.to_string()],
    })
}

/// Where a whole-project export ends, in ticks: the end of everything the timeline shows, caption
/// cues included, or `None` for a timeline that shows nothing. Raises `ValueError` for a document
/// that is not a valid project.
#[pyfunction]
fn project_end_ticks(py: Python<'_>, json: &str) -> PyResult<Option<i64>> {
    py.detach(|| Project::from_json(json))
        .map(|project| project.timeline.extent().map(|extent| extent.end().ticks()))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// An export's container, video codec and audio codec, as the host resolves them: `container`, or
/// the one `extension` (without the dot, empty for none; `None` for no output path) names; and
/// each codec as named, or the container's default. Raises `ValueError` with the engine's message
/// for a request it refuses.
#[pyfunction]
#[pyo3(signature = (container=None, extension=None, video_codec=None, audio_codec=None))]
fn resolve_export_format(
    container: Option<&str>,
    extension: Option<&str>,
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
) -> PyResult<(&'static str, &'static str, &'static str)> {
    let invalid = |error: &dyn std::fmt::Display| PyValueError::new_err(error.to_string());
    let container = container
        .map(Container::from_str)
        .transpose()
        .map_err(|error| invalid(&error))?;
    let video = video_codec
        .map(VideoCodec::from_str)
        .transpose()
        .map_err(|error| invalid(&error))?;
    let audio = audio_codec
        .map(AudioCodec::from_str)
        .transpose()
        .map_err(|error| invalid(&error))?;
    let (container, video, audio) =
        alpha2_media::resolve_export_format(container, extension, video, audio)
            .map_err(|error| invalid(&error))?;
    Ok((container.as_str(), video.as_str(), audio.as_str()))
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("SCHEMA_VERSION", CURRENT_SCHEMA_VERSION)?;
    module.add("MAX_PROJECT_JSON_BYTES", MAX_PROJECT_JSON_BYTES)?;
    module.add("TICKS_PER_SECOND", alpha2_time::TICKS_PER_SECOND)?;
    module.add_function(wrap_pyfunction!(validate_project_json, module)?)?;
    module.add_function(wrap_pyfunction!(project_end_ticks, module)?)?;
    module.add_function(wrap_pyfunction!(resolve_export_format, module)?)?;
    Ok(())
}
