//! Measurements as JSON, from generators as a project holds them: the one
//! form the wasm binding and the Python package return.

use std::fmt;

use alpha2_text::{TextRasterError, TextRasterizer};
use alpha2_time::MediaTime;

use crate::{Baselines, CanvasRect, PictureSize};

/// Why a generator could not be measured.
#[derive(Debug)]
#[non_exhaustive]
pub enum MeasureError {
    /// Not a generator of the kind measured that a project could hold, or
    /// no picture.
    Input(String),
    /// The text host refused: no fonts, say.
    Text(TextRasterError),
}

impl fmt::Display for MeasureError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) => formatter.write_str(message),
            Self::Text(error) => write!(formatter, "text failed: {error}"),
        }
    }
}

impl std::error::Error for MeasureError {}

fn baselines(baselines: Baselines) -> serde_json::Value {
    serde_json::json!({ "first": baselines.first, "last": baselines.last })
}

fn rect(rect: CanvasRect) -> serde_json::Value {
    serde_json::json!({
        "left": rect.left,
        "top": rect.top,
        "right": rect.right,
        "bottom": rect.bottom,
    })
}

/// Where the text generator `generator_json`, as a project holds it, lies on
/// a `width` by `height` canvas ([`crate::measure_text`]), as JSON
/// `{ layout, content, ink, lineCount, baselines }`: each a `{ left, top,
/// right, bottom }` rectangle in fractions of the canvas, `ink` `null` when
/// the text draws nothing, and `baselines` `{ first, last }` in fractions of
/// the canvas height.
///
/// # Errors
/// Returns [`MeasureError::Input`] for JSON that is no valid text generator
/// or an empty canvas, and [`MeasureError::Text`] for the text host's
/// refusal.
pub fn measure_text_json(
    rasterizer: &TextRasterizer,
    generator_json: &str,
    width: u32,
    height: u32,
) -> Result<String, MeasureError> {
    let generator: alpha2_model::Generator = serde_json::from_str(generator_json)
        .map_err(|error| MeasureError::Input(format!("invalid text generator: {error}")))?;
    if !generator.is_valid() {
        return Err(MeasureError::Input(
            "the text generator has a field out of range".into(),
        ));
    }
    let alpha2_model::Generator::Text { text, style } = &generator else {
        return Err(MeasureError::Input(
            "only a text generator draws text".into(),
        ));
    };
    let size = PictureSize::new(width, height)
        .map_err(|error| MeasureError::Input(format!("invalid canvas size: {error}")))?;
    let bounds =
        crate::measure_text(rasterizer, &text.text(), style, size).map_err(MeasureError::Text)?;
    Ok(serde_json::json!({
        "layout": rect(bounds.layout),
        "content": rect(bounds.content),
        "ink": bounds.ink.map(rect),
        "lineCount": bounds.line_count,
        "baselines": baselines(bounds.baselines),
    })
    .to_string())
}

/// Where each node of the layout generator `generator_json`, as a project
/// holds it, lies on a `width` by `height` picture at picture time `time`
/// ([`crate::measure_layout`]), as JSON `{ nodes }`: each node in preorder
/// as `{ at, border, content, overflow, lineCount, baselines }`, its boxes
/// in fractions of the picture, `lineCount` `null` for a node without text
/// and `baselines` (`{ first, last }`, fractions of the picture height)
/// `null` for a node without text or with empty text. `canvas` is
/// the project canvas its pixels are pixels of: the picture's size when
/// `None`.
///
/// # Errors
/// Returns [`MeasureError::Input`] for JSON that is no valid layout
/// generator on that canvas, or an empty picture or canvas, and
/// [`MeasureError::Text`] for the text host's refusal.
pub fn measure_layout_json(
    rasterizer: &TextRasterizer,
    generator_json: &str,
    width: u32,
    height: u32,
    time: MediaTime,
    canvas: Option<(u32, u32)>,
) -> Result<String, MeasureError> {
    let generator: alpha2_model::Generator = serde_json::from_str(generator_json)
        .map_err(|error| MeasureError::Input(format!("invalid layout generator: {error}")))?;
    let size = PictureSize::new(width, height)
        .map_err(|error| MeasureError::Input(format!("invalid canvas size: {error}")))?;
    let (canvas_width, canvas_height) = canvas.unwrap_or((width, height));
    PictureSize::new(canvas_width, canvas_height)
        .map_err(|error| MeasureError::Input(format!("invalid project canvas: {error}")))?;
    if let Err(issue) = generator.validate_on(canvas_width, canvas_height) {
        return Err(MeasureError::Input(match issue {
            alpha2_model::GeneratorIssue::Layout(issue) => {
                format!("the layout generator {} {}", issue.at, issue.problem)
            }
            alpha2_model::GeneratorIssue::Shape(issue) => {
                format!(
                    "the layout generator's shape {} {}",
                    issue.at, issue.problem
                )
            }
            alpha2_model::GeneratorIssue::Invalid => {
                "the layout generator has a field out of range".into()
            }
        }));
    }
    let alpha2_model::Generator::Layout { root } = &generator else {
        return Err(MeasureError::Input(
            "only a layout generator lays out boxes".into(),
        ));
    };
    let bounds = crate::measure_layout(rasterizer, root, time, size, (canvas_width, canvas_height))
        .map_err(MeasureError::Text)?;
    let nodes = bounds
        .nodes
        .into_iter()
        .map(|node| {
            serde_json::json!({
                "at": node.at,
                "border": rect(node.border),
                "content": rect(node.content),
                "overflow": node.overflow,
                "lineCount": node.line_count,
                "baselines": node.baselines.map(baselines),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "nodes": nodes }).to_string())
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
