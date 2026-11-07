//! Layout generators: box trees laid out by CSS flexbox and grid with the engine's fonts.
//!
//! A layout is resolved where text is prepared, so its text is measured and drawn by the
//! same code. It is resolved in render pixels and rounded to whole pixels. A text node
//! breaks its lines at its unrounded content width plus [`LAYOUT_WRAP_SLACK`], both when
//! measured and when drawn.

mod resolve;

#[cfg(test)]
mod tests;

use crate::fit::{BoxedLines, boxed_lines, fit_font_size, pixels, text_clamp};
use crate::text::{TextDraw, layout_text_key, stroke_pixels, text_fill};
use crate::{Baselines, CanvasRect, PictureSize};
use alpha2_eval::{EvaluatedLayout, EvaluatedNode, EvaluatedText, LayoutPlan, NodeContent};
use alpha2_media::EmojiPolicy;
use alpha2_model::{LayoutNode, TextAlign};
use alpha2_text::{ShapedText, TextClamp, TextRasterError, TextRasterRequest, TextRasterizer};
use alpha2_time::MediaTime;
use std::sync::Arc;

pub use resolve::resolve;

/// Pixels added to a text node's wrapping width so float summation order cannot re-break a
/// text sized to its own max-content width. Lines and heights may overflow the box by less
/// than this.
pub const LAYOUT_WRAP_SLACK: f32 = 1.0 / 64.0;

/// The width a text node with content `width` pixels wide breaks its lines at.
pub(crate) fn wrap(width: f32) -> f32 {
    (width + LAYOUT_WRAP_SLACK).max(1.0)
}

/// One node's boxes in render pixels, `[left, top, right, bottom]`, whole pixels from the
/// picture's top-left.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct NodeRects {
    /// The border box.
    pub border: [f32; 4],
    /// The padding box.
    pub padding: [f32; 4],
    /// The content box.
    pub content: [f32; 4],
    /// The unrounded content width a text breaks at, before the slack.
    pub text_width: f32,
    /// The unrounded content height a text fits and is clamped in, before the slack.
    pub text_height: f32,
    /// A text node's font size as a fraction of the picture's shorter side, fitted when it
    /// has a `font_size_fit` ([`fit_texts`]); zero for other nodes.
    pub font_size: f32,
    /// A text node's font size in pixels, as the text host draws it.
    pub font_pixels: f32,
    /// Whether a text node's `line_clamp` discarded lines.
    pub truncated: bool,
    /// Whether it overflows, as [`NodeBounds::overflow`] says.
    pub overflow: bool,
    /// A text node's line count, zero for empty text.
    pub line_count: Option<u32>,
    /// A text node's first and last baselines in pixels from the picture's top. `None` for
    /// other nodes and empty text.
    pub baselines: Option<[f32; 2]>,
}

/// Every node's boxes, in preorder ([`resolve`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedLayout {
    /// Each node, the root first.
    pub nodes: Vec<NodeRects>,
}

/// Where a layout's nodes lie ([`measure_layout`]), in preorder.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutBounds {
    /// Each node, the root first.
    pub nodes: Vec<NodeBounds>,
}

/// Where one layout node's boxes lie, before its clip's transform and node visuals.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeBounds {
    /// Its path from the root, like `root.children[1]`.
    pub at: String,
    /// The border box, as fractions of the picture.
    pub border: CanvasRect,
    /// The content box, as fractions of the picture.
    pub content: CanvasRect,
    /// Whether it overflows: its text (at its fitted size, once clamped) is taller than its
    /// content box or splits a word, or an in-flow child reaches past its padding box.
    pub overflow: bool,
    /// A text node's line count; `None` for other nodes.
    pub line_count: Option<u32>,
    /// A text node's font size as drawn, a fraction of the picture's shorter side; `None` for
    /// other nodes.
    pub font_size: Option<f32>,
    /// Whether a text node's `line_clamp` discarded lines.
    pub truncated: bool,
    /// A text node's first and last baselines as fractions of the picture height. `None` for
    /// other nodes and empty text.
    pub baselines: Option<Baselines>,
}

/// The text host request for a layout's text at `font_size` pixels, wrapped at `max_width`
/// and cut by `clamp`.
pub(crate) fn text_request(
    text: &EvaluatedText,
    font_size: f32,
    max_width: f32,
    clamp: Option<TextClamp>,
    content_version: u64,
) -> TextRasterRequest {
    TextRasterRequest {
        text: text.content.clone(),
        font_size,
        max_width,
        line_height: text.line_height,
        align: match text.text_align {
            TextAlign::Start => alpha2_media::TextAlign::Start,
            TextAlign::Center => alpha2_media::TextAlign::Center,
            TextAlign::End => alpha2_media::TextAlign::End,
        },
        fill: text_fill(&text.fill),
        emoji_policy: EmojiPolicy::Color,
        content_version,
        strokes: stroke_pixels(&text.strokes, font_size),
        background: None,
        split_background: false,
        font_family: text.font_family.clone(),
        letter_spacing: text.letter_spacing * font_size,
        font_weight: text.font_weight,
        clamp,
    }
}

/// How `shaped` lies in a text node's content box `width` by `height` unrounded pixels,
/// cut by its clamp. Lines break at the width plus the slack and fit in the height plus the
/// slack.
pub(crate) fn node_lines(
    text: &EvaluatedText,
    shaped: &ShapedText,
    width: f32,
    height: f32,
) -> BoxedLines {
    boxed_lines(
        shaped,
        wrap(width),
        height + LAYOUT_WRAP_SLACK,
        text.line_clamp.as_ref().and_then(|clamp| clamp.max_lines),
        text.line_clamp.is_some(),
    )
}

/// Fits the font size of each text node with a `font_size_fit` to its content box in
/// `resolved` ([`resolve`]), with lines from `shape`, and updates its font size, lines and
/// overflow. The boxes keep the sizes they were laid out at.
///
/// # Errors
/// Returns the first refusal of `shape`.
pub fn fit_texts(
    layout: &EvaluatedLayout,
    resolved: &mut ResolvedLayout,
    size: PictureSize,
    mut shape: impl FnMut(&TextRasterRequest) -> Result<Arc<ShapedText>, TextRasterError>,
) -> Result<(), TextRasterError> {
    let unit = size.unit();
    let mut found = Vec::new();
    text_nodes(&layout.root, &mut 0, &mut found);
    for (node, text) in found {
        let (Some(text), Some(rects)) = (text, resolved.nodes.get_mut(node)) else {
            continue;
        };
        if text.font_size_fit.is_none() {
            continue;
        }
        let (width, height) = (rects.text_width, rects.text_height);
        let request = |pixels| text_request(text, pixels, 0.0, None, 1);
        let fitted = fit_font_size(text.font_size, text.font_size_fit, unit, |pixels| {
            Ok(node_lines(text, &*shape(&request(pixels))?, width, height).fits)
        })?;
        let lines = node_lines(text, &*shape(&request(fitted.font_pixels))?, width, height);
        let top = rects.content[1];
        rects.font_size = fitted.font_size;
        rects.font_pixels = fitted.font_pixels;
        rects.truncated = lines.truncated;
        rects.line_count = Some(lines.lines.line_count);
        rects.baselines = Some([
            top + lines.lines.first_baseline,
            top + lines.lines.last_baseline,
        ]);
        rects.overflow = lines.overflow;
    }
    Ok(())
}

/// Each text node's preorder index and its text, `None` for empty text.
fn text_nodes<'a>(
    node: &'a EvaluatedNode,
    index: &mut usize,
    out: &mut Vec<(usize, Option<&'a EvaluatedText>)>,
) {
    let at = *index;
    *index += 1;
    match &node.content {
        NodeContent::Text(text) => {
            out.push((at, Some(&**text).filter(|text| !text.content.is_empty())));
        }
        NodeContent::Children(children) => {
            for child in children {
                text_nodes(child, index, out);
            }
        }
        NodeContent::Empty | NodeContent::Shape(_) => {}
    }
}

/// Each text node's text in preorder; `None` for empty text, which measures and draws
/// nothing.
pub(crate) fn texts<'a>(node: &'a EvaluatedNode, out: &mut Vec<Option<&'a EvaluatedText>>) {
    match &node.content {
        NodeContent::Text(text) => out.push(Some(&**text).filter(|text| !text.content.is_empty())),
        NodeContent::Children(children) => {
            for child in children {
                texts(child, out);
            }
        }
        NodeContent::Empty | NodeContent::Shape(_) => {}
    }
}

/// Shapes each text node's text in preorder with `shape` on a picture of `size`, as
/// [`resolve`] lays them out.
///
/// # Errors
/// Returns the first refusal of `shape`.
pub fn shape_texts(
    layout: &EvaluatedLayout,
    size: PictureSize,
    mut shape: impl FnMut(&TextRasterRequest) -> Result<Arc<ShapedText>, TextRasterError>,
) -> Result<Vec<Option<Arc<ShapedText>>>, TextRasterError> {
    let unit = size.unit();
    let mut list = Vec::new();
    texts(&layout.root, &mut list);
    list.into_iter()
        .map(|text| {
            text.map(|text| {
                shape(&text_request(
                    text,
                    pixels(text.font_size, unit),
                    0.0,
                    None,
                    1,
                ))
            })
            .transpose()
        })
        .collect()
}

/// Each text node's store key in preorder (`None` for empty text), and each distinct key's
/// raster request at its font size in `resolved`, wrapped at its unrounded content width plus
/// the slack and clamped in its content height plus the slack.
#[must_use]
pub fn layout_texts(
    layout: &EvaluatedLayout,
    resolved: &ResolvedLayout,
    size: PictureSize,
) -> (Vec<Option<u64>>, Vec<(u64, TextDraw)>) {
    let unit = size.unit();
    let mut found = Vec::new();
    text_nodes(&layout.root, &mut 0, &mut found);
    let mut keys = Vec::with_capacity(found.len());
    let mut requests: Vec<(u64, TextDraw)> = Vec::new();
    for (node, text) in found {
        let Some(text) = text else {
            keys.push(None);
            continue;
        };
        let rects = resolved.nodes.get(node).copied().unwrap_or_default();
        let font_size = if rects.font_pixels > 0.0 {
            rects.font_pixels
        } else {
            pixels(text.font_size, unit)
        };
        let clamp = text
            .line_clamp
            .as_ref()
            .map(|clamp| text_clamp(clamp, rects.text_height + LAYOUT_WRAP_SLACK));
        let mut request = text_request(text, font_size, wrap(rects.text_width), clamp, 1);
        let shadows = crate::text::shadow_pixels(&text.shadows, request.font_size);
        let key = layout_text_key(&request, &shadows, &text.animators);
        request.content_version = key;
        if !requests.iter().any(|(held, _)| *held == key) {
            let animation = (!text.animators.is_empty()).then(|| crate::TextAnimation {
                animators: text.animators.clone(),
                runs: Vec::new(),
                time: layout.time,
            });
            requests.push((
                key,
                TextDraw {
                    request,
                    shadows,
                    animation,
                },
            ));
        }
        keys.push(Some(key));
    }
    (keys, requests)
}

/// Where each node of a valid layout tree lies on a picture of `size` at picture time
/// `time`, laid out with `rasterizer`'s fonts exactly as a frame is.
///
/// `canvas` is the project canvas the tree's pixel lengths refer to. Rects are layout boxes,
/// before the clip's transform and node visuals. An invalid tree gives unspecified rects but
/// never panics.
///
/// # Errors
/// Returns the text host's refusal of a text: no fonts, or a size out of range.
#[allow(clippy::cast_precision_loss)]
pub fn measure_layout(
    rasterizer: &TextRasterizer,
    root: &LayoutNode,
    time: MediaTime,
    size: PictureSize,
    canvas: (u32, u32),
) -> Result<LayoutBounds, TextRasterError> {
    let evaluated = LayoutPlan::new(root, canvas).evaluate(time);
    let shaped = shape_texts(&evaluated, size, |request| {
        rasterizer.shape(request).map(Arc::new)
    })?;
    let mut resolved = resolve(&evaluated, size, &shaped);
    fit_texts(&evaluated, &mut resolved, size, |request| {
        rasterizer.shape(request).map(Arc::new)
    })?;
    let (width, height) = (size.width() as f32, size.height() as f32);
    let rect = |[left, top, right, bottom]: [f32; 4]| CanvasRect {
        left: left / width,
        top: top / height,
        right: right / width,
        bottom: bottom / height,
    };
    let mut paths = Vec::new();
    root.walk(|at, _| paths.push(at.to_owned()));
    Ok(LayoutBounds {
        nodes: paths
            .into_iter()
            .zip(&resolved.nodes)
            .map(|(at, node)| NodeBounds {
                at,
                border: rect(node.border),
                content: rect(node.content),
                overflow: node.overflow,
                line_count: node.line_count,
                font_size: node.line_count.map(|_| node.font_size),
                truncated: node.truncated,
                baselines: node.baselines.map(|[first, last]| Baselines {
                    first: first / height,
                    last: last / height,
                }),
            })
            .collect(),
    })
}
