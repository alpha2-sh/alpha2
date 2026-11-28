//! Measurements as JSON, in the form the wasm binding and the Python package return.

use std::fmt;

use alpha2_text::{TextRasterError, TextRasterizer};
use alpha2_time::MediaTime;

use crate::{Baselines, CanvasRect, PictureSize};

/// Why a generator could not be measured.
#[derive(Debug)]
#[non_exhaustive]
pub enum MeasureError {
    /// Not a valid generator of the measured kind, or an empty picture.
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

/// Measures the text generator `generator_json` on a `width` by `height` canvas
/// ([`crate::measure_text`]).
///
/// Returns JSON `{ layout, content, ink, lineCount, baselines, fontSize, truncated, overflow }`,
/// and `advanceHeadroom` (see [`crate::TextBounds::advance_headroom`]) when it is not 1.
/// Rectangles are `{ left, top, right, bottom }` in fractions of the canvas, `ink` is `null`
/// when the text draws nothing, and `baselines` is `{ first, last }` in fractions of the canvas
/// height. `fontSize` is the size drawn after `font_size_fit`, a fraction of the canvas's
/// shorter side. `truncated` says `line_clamp` cut lines, and `overflow` that the drawn text
/// still splits a word or is taller than `max_height`.
///
/// # Errors
/// Returns [`MeasureError::Input`] for invalid generator JSON or an empty canvas, and
/// [`MeasureError::Text`] for the text host's refusal.
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
    let bounds = crate::measure_text(
        rasterizer,
        &crate::TextAt::new(text, MediaTime::ZERO),
        style,
        size,
    )
    .map_err(MeasureError::Text)?;
    let mut measured = serde_json::json!({
        "layout": rect(bounds.layout),
        "content": rect(bounds.content),
        "ink": bounds.ink.map(rect),
        "lineCount": bounds.line_count,
        "baselines": baselines(bounds.baselines),
        "fontSize": bounds.font_size,
        "truncated": bounds.truncated,
        "overflow": bounds.overflow,
    });
    with_headroom(&mut measured, Some(bounds.advance_headroom));
    Ok(measured.to_string())
}

/// Adds `advanceHeadroom` to `measured` when animators reserve some.
fn with_headroom(measured: &mut serde_json::Value, headroom: Option<f32>) {
    if let Some(headroom) = headroom.filter(|headroom| *headroom > 1.0) {
        measured["advanceHeadroom"] = serde_json::json!(headroom);
    }
}

/// The unit a report counts a text's positions in: each binding's string unit.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum StringUnit {
    /// UTF-8 bytes (Rust).
    Utf8,
    /// UTF-16 code units (JavaScript).
    Utf16,
    /// Unicode scalar values (Python).
    CodePoints,
}

impl StringUnit {
    /// The position of each UTF-8 byte offset of `text` in this unit, plus one for its end.
    fn positions(self, text: &str) -> Vec<usize> {
        let mut positions = vec![0; text.len() + 1];
        let mut at = 0;
        for (start, character) in text.char_indices() {
            let width = match self {
                Self::Utf8 => character.len_utf8(),
                Self::Utf16 => character.len_utf16(),
                Self::CodePoints => 1,
            };
            for slot in &mut positions[start..start + character.len_utf8()] {
                *slot = at;
            }
            at += width;
        }
        positions[text.len()] = at;
        positions
    }
}

/// Reports where each line, word and cluster of the text generator `generator_json` lies on a
/// `width` by `height` canvas ([`crate::text_layout`]), ranges in `unit`.
///
/// Returns JSON `{ fontSize, layout, lines, words, clusters }`: lines as `{ clusters: [first,
/// end], box, em, baseline }`, words as `{ clusters, boxes, ink }`, clusters as `{ range:
/// [start, end], run, word, line, space, ellipsis, fontSize, baseline, box, ink }`. Rectangles
/// are `{ left, top, right, bottom }` in fractions of the canvas, baselines fractions of its
/// height, font sizes fractions of its shorter side; `run`, `word` and `ink` may be `null`.
///
/// # Errors
/// Returns [`MeasureError::Input`] for invalid generator JSON or an empty canvas, and
/// [`MeasureError::Text`] for the text host's refusal.
pub fn text_layout_json(
    rasterizer: &TextRasterizer,
    generator_json: &str,
    width: u32,
    height: u32,
    unit: StringUnit,
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
    let report = crate::text_layout(
        rasterizer,
        &crate::TextAt::new(text, MediaTime::ZERO),
        style,
        size,
    )
    .map_err(MeasureError::Text)?;
    let positions = unit.positions(&text.text());
    let at = |byte: usize| positions.get(byte).copied().unwrap_or(0);
    let span = |range: &std::ops::Range<usize>| serde_json::json!([range.start, range.end]);
    Ok(serde_json::json!({
        "fontSize": report.font_size,
        "layout": rect(report.layout),
        "lines": report.lines.iter().map(|line| serde_json::json!({
            "clusters": span(&line.clusters),
            "box": rect(line.line_box),
            "em": rect(line.em),
            "baseline": line.baseline,
        })).collect::<Vec<_>>(),
        "words": report.words.iter().map(|word| serde_json::json!({
            "clusters": span(&word.clusters),
            "boxes": word.boxes.iter().copied().map(rect).collect::<Vec<_>>(),
            "ink": word.ink.map(rect),
        })).collect::<Vec<_>>(),
        "clusters": report.clusters.iter().map(|cluster| serde_json::json!({
            "range": [at(cluster.bytes.start), at(cluster.bytes.end)],
            "run": cluster.run,
            "word": cluster.word,
            "line": cluster.line,
            "space": cluster.space,
            "ellipsis": cluster.ellipsis,
            "fontSize": cluster.font_size,
            "baseline": cluster.baseline,
            "box": rect(cluster.cluster_box),
            "ink": cluster.ink.map(rect),
        })).collect::<Vec<_>>(),
    })
    .to_string())
}

/// Measures each node of the layout generator `generator_json` on a `width` by `height`
/// picture at picture time `time` ([`crate::measure_layout`]).
///
/// Returns JSON `{ nodes }`, each node in preorder as
/// `{ at, border, content, overflow, lineCount, baselines, fontSize, truncated }`, and
/// `advanceHeadroom` for a text node whose animators reserve some, with boxes in
/// fractions of the picture. `lineCount` and `fontSize` (the size drawn, a fraction of the
/// picture's shorter side) are `null` for non-text nodes, and `baselines` (`{ first, last }`,
/// fractions of the picture height) also for empty text. `truncated` says `line_clamp` cut the
/// node's lines; `overflow` is also true for a text the text host refuses to draw, such as a box
/// over 16,384 pixels a side. `canvas` is the project canvas the generator's pixel lengths refer
/// to, the picture's size when `None`.
///
/// # Errors
/// Returns [`MeasureError::Input`] for an invalid layout generator on that canvas or an empty
/// picture or canvas, and [`MeasureError::Text`] for the text host's refusal.
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
            alpha2_model::GeneratorIssue::Invalid
            | alpha2_model::GeneratorIssue::TextRun { .. } => {
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
            let mut measured = serde_json::json!({
                "at": node.at,
                "border": rect(node.border),
                "content": rect(node.content),
                "overflow": node.overflow,
                "lineCount": node.line_count,
                "baselines": node.baselines.map(baselines),
                "fontSize": node.font_size,
                "truncated": node.truncated,
            });
            with_headroom(&mut measured, node.advance_headroom);
            measured
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({ "nodes": nodes }).to_string())
}

#[cfg(test)]
#[path = "json_tests.rs"]
mod tests;
