//! `alpha2._native`, the compiled part of the `alpha2` Python package.
//!
//! It gives Python the engine's own answers where a second implementation would drift: whether a
//! project document is valid (`alpha2_model`, the validator the host runs), where a project ends,
//! which container and codecs an export resolves to (`alpha2_media`), what a compose spec
//! compiles to (`alpha2_compose`, the compiler every binding shares), and where text and layout
//! boxes lie (`alpha2_layout`, the placement the engine draws by). It links neither a GPU nor
//! `FFmpeg`; rendering is the `alpha2-host` process's.
//!
//! Documents cross the boundary as JSON text, which the Python side writes once with the same
//! rules it sends the host.

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

/// A valid project's non-fatal diagnostics (`Project::lint`), as a JSON array of issues. Raises
/// `ValueError` for a document that is not a valid project.
#[pyfunction]
fn lint_project_json(py: Python<'_>, json: &str) -> PyResult<String> {
    py.detach(|| {
        let project =
            Project::from_json(json).map_err(|error| PyValueError::new_err(error.to_string()))?;
        serde_json::to_string(&project.lint())
            .map_err(|error| PyValueError::new_err(error.to_string()))
    })
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

/// The pictures `video_codec` writes in `container` at `bit_depth` (8 or 10; `None` for the codec's
/// own) with or without `alpha`, as the host resolves them: the `FFmpeg` pixel format name, such as
/// `yuva420p`. Raises `ValueError` with the engine's message for pictures the codec cannot write.
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

/// A compose error as `ValueError(message, path, problem)`: the whole message, where in the spec,
/// and what is wrong there.
fn compose_error(error: &alpha2_compose::ComposeError) -> PyErr {
    PyValueError::new_err((error.to_string(), error.path.clone(), error.message.clone()))
}

/// The files a compose spec (JSON text) names, as JSON text: `{"media": [{path, kind}], "texts":
/// [path]}`. Raises `ValueError(message, path, problem)` for a spec that does not parse.
#[pyfunction]
fn compose_requirements(py: Python<'_>, spec: &str) -> PyResult<String> {
    py.detach(|| alpha2_compose::requirements_json(spec))
        .map_err(|error| compose_error(&error))
}

/// A compose spec compiled with facts about its files (both JSON text) and the font files an
/// export registers, as JSON text: `{"project", "assets", "loudness", "warnings"}`. Raises
/// `ValueError(message, path, problem)` for a spec that does not compile.
#[pyfunction]
fn compose(py: Python<'_>, spec: &str, facts: &str, fonts: Vec<Vec<u8>>) -> PyResult<String> {
    py.detach(|| {
        let fonts = fonts.into_iter().map(Arc::from).collect::<Vec<Arc<[u8]>>>();
        alpha2_compose::compile_json(spec, facts, &fonts)
    })
    .map_err(|error| compose_error(&error))
}

/// A text and layout measurer over the export's font files, registered as the host registers
/// them (every face of a collection, in order). Measuring only reads it, so threads share one.
#[pyclass(module = "alpha2._native", frozen)]
struct TextMeasurer {
    inner: TextRasterizer,
}

/// A font file the measurer cannot register, as the error Python raises for it: a missing file as
/// `FileNotFoundError`, another read failure as `OSError`, a file with no face drawn as
/// `ValueError`.
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
    /// A measurer drawing with the font files at `fonts`, in fallback order. Raises
    /// `FileNotFoundError` naming a missing file and `ValueError` naming one with no face drawn.
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

    /// Where the text generator `generator` (JSON text) lies on a `width` by `height` canvas, as
    /// JSON text `{layout, content, ink, lineCount, baselines}`. Raises `ValueError` with the
    /// engine's message for a generator it refuses.
    fn text(&self, py: Python<'_>, generator: &str, width: u32, height: u32) -> PyResult<String> {
        py.detach(|| measure_text_json(&self.inner, generator, width, height))
            .map_err(|error| measure_error(&error))
    }

    /// Where each node of the layout generator `generator` (JSON text) lies on a `width` by
    /// `height` picture at picture time `time` (ticks), its `"<n>px"` lengths pixels of `canvas`
    /// (the picture's size when `None`), as JSON text `{nodes}`. Raises `ValueError` with the
    /// engine's message for a generator it refuses.
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
    module.add_class::<TextMeasurer>()?;
    Ok(())
}
