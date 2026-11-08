//! `alpha2._native`, the compiled part of the `alpha2` Python package.
//!
//! Exposes the engine's own validation, export format resolution, compose compiler and text
//! layout so Python does not reimplement them. Links neither a GPU nor `FFmpeg`. Documents cross
//! the boundary as JSON text.

use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use alpha2_layout::FontFileError;
use alpha2_layout::json::{MeasureError, measure_layout_json, measure_text_json};
use alpha2_media::{AudioCodec, Container, VideoCodec};
use alpha2_model::{CURRENT_SCHEMA_VERSION, MAX_PROJECT_JSON_BYTES, Project};
use alpha2_text::TextRasterizer;
use alpha2_time::MediaTime;
use pyo3::exceptions::{PyFileNotFoundError, PyOSError, PyValueError};
use pyo3::prelude::*;

/// Validation errors for a project document; empty when valid, at most one entry.
#[pyfunction]
fn validate_project_json(py: Python<'_>, json: &str) -> Vec<String> {
    py.detach(|| match Project::from_json(json) {
        Ok(_) => Vec::new(),
        Err(error) => vec![error.to_string()],
    })
}

/// End of a whole-project export in ticks, caption cues included, or `None` for an empty timeline.
/// Raises `ValueError` for an invalid project.
#[pyfunction]
fn project_end_ticks(py: Python<'_>, json: &str) -> PyResult<Option<i64>> {
    py.detach(|| Project::from_json(json))
        .map(|project| project.extent().map(|extent| extent.end().ticks()))
        .map_err(|error| PyValueError::new_err(error.to_string()))
}

/// Non-fatal diagnostics (`Project::lint`) as a JSON array. Raises `ValueError` for an invalid
/// project.
#[pyfunction]
fn lint_project_json(py: Python<'_>, json: &str) -> PyResult<String> {
    py.detach(|| {
        let project =
            Project::from_json(json).map_err(|error| PyValueError::new_err(error.to_string()))?;
        serde_json::to_string(&project.lint())
            .map_err(|error| PyValueError::new_err(error.to_string()))
    })
}

/// Resolves an export's `(container, video codec, audio codec)` as the host does. `extension` has
/// no dot (empty for none, `None` for no output path); unnamed codecs take the container's
/// default. Raises `ValueError` for a refused request.
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

/// The `FFmpeg` pixel format (such as `yuva420p`) `video_codec` writes in `container`.
/// `bit_depth` is 8 or 10, or `None` for the codec's default. Raises `ValueError` for pixels the
/// codec cannot write.
#[pyfunction]
#[pyo3(signature = (video_codec, container, bit_depth=None, alpha=false))]
fn resolve_pixels(
    video_codec: &str,
    container: &str,
    bit_depth: Option<u8>,
    alpha: bool,
) -> PyResult<&'static str> {
    let invalid = |error: &dyn std::fmt::Display| PyValueError::new_err(error.to_string());
    let codec = VideoCodec::from_str(video_codec).map_err(|error| invalid(&error))?;
    let container = Container::from_str(container).map_err(|error| invalid(&error))?;
    let bit_depth = bit_depth
        .map(|bits| {
            alpha2_media::BitDepth::from_bits(bits)
                .ok_or_else(|| PyValueError::new_err(format!("bit_depth: {bits} is not 8 or 10")))
        })
        .transpose()?;
    alpha2_media::resolve_pixels(
        codec,
        container,
        alpha2_media::OutputPixels { bit_depth, alpha },
    )
    .map(alpha2_media::PixelLayout::ffmpeg_name)
    .map_err(|error| invalid(&error))
}

/// Maps a compose error to `ValueError(message, path, problem)`.
fn compose_error(error: &alpha2_compose::ComposeError) -> PyErr {
    PyValueError::new_err((error.to_string(), error.path.clone(), error.message.clone()))
}

/// Files a compose spec names, as JSON `{"media": [{path, kind}], "texts": [path]}`. Raises
/// `ValueError(message, path, problem)` for a spec that does not parse.
#[pyfunction]
fn compose_requirements(py: Python<'_>, spec: &str) -> PyResult<String> {
    py.detach(|| alpha2_compose::requirements_json(spec))
        .map_err(|error| compose_error(&error))
}

/// Compiles a compose spec with its file facts and export fonts to JSON
/// `{"project", "assets", "loudness", "warnings"}`. Raises `ValueError(message, path, problem)`.
#[pyfunction]
fn compose(py: Python<'_>, spec: &str, facts: &str, fonts: Vec<Vec<u8>>) -> PyResult<String> {
    py.detach(|| {
        let fonts = fonts.into_iter().map(Arc::from).collect::<Vec<Arc<[u8]>>>();
        alpha2_compose::compile_json(spec, facts, &fonts)
    })
    .map_err(|error| compose_error(&error))
}

/// Binds a template (or expands a spec's template nodes) to its data, giving a plain spec or
/// node as JSON. `options` is `{"theme"?, "templates"?}`. Raises `ValueError(message, path,
/// problem)`.
#[pyfunction]
fn compose_bind(py: Python<'_>, document: &str, data: &str, options: &str) -> PyResult<String> {
    py.detach(|| alpha2_compose::bind_json(document, data, options))
        .map_err(|error| compose_error(&error))
}

/// A template's data as a JSON Schema. Raises `ValueError(message, path, problem)`.
#[pyfunction]
fn compose_data_schema(py: Python<'_>, template: &str) -> PyResult<String> {
    py.detach(|| alpha2_compose::data_schema_json(template))
        .map_err(|error| compose_error(&error))
}

/// A template's problems, with `data` when not empty, as a JSON list of
/// `{path, code, message, severity}`. Raises `ValueError` for text that is not JSON.
#[pyfunction]
fn compose_lint(py: Python<'_>, template: &str, data: &str, options: &str) -> PyResult<String> {
    py.detach(|| alpha2_compose::lint_json(template, data, options))
        .map_err(|error| compose_error(&error))
}

/// The compose schema without engine nodes (`raw`, `layout`), as JSON.
#[pyfunction]
fn compose_core_schema() -> String {
    alpha2_compose::compose_core_json_schema().to_string()
}

/// Text and layout measurer over the export's fonts, registered in the same order as the host.
#[pyclass(module = "alpha2._native", frozen)]
struct TextMeasurer {
    inner: TextRasterizer,
}

fn font_error(error: &FontFileError) -> PyErr {
    match error {
        FontFileError::Read { error: io, .. } if io.kind() == std::io::ErrorKind::NotFound => {
            PyFileNotFoundError::new_err(error.to_string())
        }
        FontFileError::Read { .. } => PyOSError::new_err(error.to_string()),
        _ => PyValueError::new_err(error.to_string()),
    }
}

fn measure_error(error: &MeasureError) -> PyErr {
    PyValueError::new_err(error.to_string())
}

#[pymethods]
impl TextMeasurer {
    /// Loads `fonts` in fallback order. Raises `FileNotFoundError` for a missing file and
    /// `ValueError` for a file with no usable face.
    #[new]
    #[allow(
        clippy::needless_pass_by_value,
        reason = "Python arguments arrive owned"
    )]
    fn new(py: Python<'_>, fonts: Vec<PathBuf>) -> PyResult<Self> {
        py.detach(|| {
            let files =
                alpha2_layout::fonts_from_files(&fonts).map_err(|error| font_error(&error))?;
            let mut inner = TextRasterizer::empty();
            for face in files.faces {
                inner
                    .add_font(face)
                    .map_err(|error| PyValueError::new_err(error.to_string()))?;
            }
            Ok(Self { inner })
        })
    }

    /// Measures a text generator (JSON) on a `width` by `height` canvas, returning JSON
    /// `{layout, content, ink, lineCount, baselines}`. Raises `ValueError` for a refused generator.
    fn text(&self, py: Python<'_>, generator: &str, width: u32, height: u32) -> PyResult<String> {
        py.detach(|| measure_text_json(&self.inner, generator, width, height))
            .map_err(|error| measure_error(&error))
    }

    /// Places each node of a layout generator (JSON) at `time` ticks, returning JSON `{nodes}`.
    /// `"<n>px"` lengths are pixels of `canvas`, which defaults to `width` by `height`. Raises
    /// `ValueError` for a refused generator.
    #[pyo3(signature = (generator, width, height, time, canvas=None))]
    fn layout(
        &self,
        py: Python<'_>,
        generator: &str,
        width: u32,
        height: u32,
        time: i64,
        canvas: Option<(u32, u32)>,
    ) -> PyResult<String> {
        py.detach(|| {
            measure_layout_json(
                &self.inner,
                generator,
                width,
                height,
                MediaTime::from_ticks(time),
                canvas,
            )
        })
        .map_err(|error| measure_error(&error))
    }
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("SCHEMA_VERSION", CURRENT_SCHEMA_VERSION)?;
    module.add("MAX_PROJECT_JSON_BYTES", MAX_PROJECT_JSON_BYTES)?;
    module.add("TICKS_PER_SECOND", alpha2_time::TICKS_PER_SECOND)?;
    module.add_function(wrap_pyfunction!(validate_project_json, module)?)?;
    module.add_function(wrap_pyfunction!(project_end_ticks, module)?)?;
    module.add_function(wrap_pyfunction!(lint_project_json, module)?)?;
    module.add_function(wrap_pyfunction!(resolve_export_format, module)?)?;
    module.add_function(wrap_pyfunction!(resolve_pixels, module)?)?;
    module.add("COMPOSE_VERSION", alpha2_compose::COMPOSE_VERSION)?;
    module.add("COMPOSE_THEMES", alpha2_compose::THEMES.to_vec())?;
    module.add_function(wrap_pyfunction!(compose_requirements, module)?)?;
    module.add_function(wrap_pyfunction!(compose, module)?)?;
    module.add_function(wrap_pyfunction!(compose_bind, module)?)?;
    module.add_function(wrap_pyfunction!(compose_data_schema, module)?)?;
    module.add_function(wrap_pyfunction!(compose_lint, module)?)?;
    module.add_function(wrap_pyfunction!(compose_core_schema, module)?)?;
    module.add("COMPOSE_TEMPLATES", alpha2_compose::templates_json())?;
    module.add_class::<TextMeasurer>()?;
    Ok(())
}
