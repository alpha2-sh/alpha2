//! Layout generators: box trees laid out by CSS flexbox with the fonts the
//! engine draws with, then drawn from vector fills and text textures.
//!
//! A layout is resolved where text is prepared ([`crate::TextTextures`]),
//! which holds the fonts, so its text is measured and drawn by the same
//! code. It is resolved at the render size, in render pixels, and rounded
//! to whole pixels; a text node breaks its lines at its unrounded content
//! width plus [`LAYOUT_WRAP_SLACK`], when measured and when drawn alike.

pub(crate) mod draw;
pub use draw::PreparedLayout;
pub(crate) mod paint;
mod resolve;

#[cfg(test)]
mod tests;

use crate::text::{MAX_FONT_PIXELS, MAX_OUTLINE_PIXELS, rgba8};
use crate::{CanvasRect, TextError};
use alpha2_eval::{EvaluatedLayout, EvaluatedNode, EvaluatedText, LayoutPlan, NodeContent};
use alpha2_media::EmojiPolicy;
use alpha2_model::{LayoutNode, TextAlign};
use alpha2_render::RenderSize;
use alpha2_text::{ShapedText, TextRasterError, TextRasterRequest, TextRasterizer};
use alpha2_time::MediaTime;
use std::sync::Arc;

pub(crate) use resolve::resolve;

/// What a text node's wrapping width adds to its content width, in pixels:
/// float summation order cannot then break a text sized to its own
/// max-content width again. A line it lets through overflows its box by
/// under this much.
pub const LAYOUT_WRAP_SLACK: f32 = 1.0 / 64.0;

/// The width a text node with content `width` pixels wide breaks its lines
/// at, when measured and when drawn.
pub(crate) fn wrap(width: f32) -> f32 {
    (width + LAYOUT_WRAP_SLACK).max(1.0)
}

/// One node's boxes in render pixels, `[left, top, right, bottom]`, whole
/// pixels from the picture's top-left corner.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub(crate) struct NodeRects {
    pub(crate) border: [f32; 4],
    pub(crate) padding: [f32; 4],
    pub(crate) content: [f32; 4],
    /// The unrounded content width a text breaks at, before the slack.
    pub(crate) text_width: f32,
    pub(crate) overflow: bool,
    /// A text node's line count, zero for empty text.
    pub(crate) line_count: Option<u32>,
}

/// Every node's boxes, in preorder.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ResolvedLayout {
    pub(crate) nodes: Vec<NodeRects>,
}

/// Where a layout's nodes lie ([`measure_layout`]), in preorder.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutBounds {
    /// Each node, the root first.
    pub nodes: Vec<NodeBounds>,
}

/// Where one layout node lies, before its clip's transform and before node
/// visuals: its layout boxes, not where it is drawn.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeBounds {
    /// Its path from the root, like `root.children[1]`.
    pub at: String,
    /// The border box, as fractions of the picture.
    pub border: CanvasRect,
    /// The content box, as fractions of the picture.
    pub content: CanvasRect,
    /// Whether it overflows: its text is taller than its content box or
    /// splits a word, or an in-flow child reaches past its padding box.
    pub overflow: bool,
    /// A text node's line count; `None` for other nodes.
    pub line_count: Option<u32>,
}

/// The font size in pixels of `text` on a picture whose shorter side is
/// `unit` pixels, within what the text host draws.
pub(crate) fn font_pixels(text: &EvaluatedText, unit: f32) -> f32 {
    (text.font_size * unit).clamp(1.0, MAX_FONT_PIXELS)
}

/// The text host request for a layout's text, wrapped at `max_width`.
pub(crate) fn text_request(
    text: &EvaluatedText,
    unit: f32,
    max_width: f32,
    content_version: u64,
) -> TextRasterRequest {
    let font_size = font_pixels(text, unit);
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
        color: rgba8(text.color),
        emoji_policy: EmojiPolicy::Color,
        content_version,
        outline: text.outline.map(|outline| alpha2_text::TextOutline {
            color: rgba8(outline.color),
            width: (outline.width * font_size).clamp(f32::MIN_POSITIVE, MAX_OUTLINE_PIXELS),
        }),
        background: None,
        font_family: text.font_family.clone(),
    }
}

/// The shorter side of `size` in pixels: what a layout's lengths are
/// fractions of.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn unit_of(size: RenderSize) -> f32 {
    size.width.min(size.height) as f32
}

/// Each text node's text in preorder: `None` for empty text, which measures
/// nothing and draws nothing.
pub(crate) fn texts<'a>(node: &'a EvaluatedNode, out: &mut Vec<Option<&'a EvaluatedText>>) {
    match &node.content {
        NodeContent::Text(text) => out.push(Some(text).filter(|text| !text.content.is_empty())),
        NodeContent::Children(children) => {
            for child in children {
                texts(child, out);
            }
        }
        NodeContent::Empty | NodeContent::Shape(_) => {}
    }
}

/// Each text node's shaped text, in preorder, shaped by `shape`.
pub(crate) fn shape_texts(
    layout: &EvaluatedLayout,
    size: RenderSize,
    mut shape: impl FnMut(&TextRasterRequest) -> Result<Arc<ShapedText>, TextRasterError>,
) -> Result<Vec<Option<Arc<ShapedText>>>, TextError> {
    let unit = unit_of(size);
    let mut list = Vec::new();
    texts(&layout.root, &mut list);
    list.into_iter()
        .map(|text| {
            text.map(|text| shape(&text_request(text, unit, 0.0, 1)))
                .transpose()
                .map_err(TextError::Raster)
        })
        .collect()
}

/// Each text node's store key in preorder (`None` for empty text), and each
/// distinct key's raster request: wrapped at its unrounded content width
/// plus the slack, as it was measured.
pub(crate) fn layout_texts(
    layout: &EvaluatedLayout,
    resolved: &ResolvedLayout,
    size: RenderSize,
) -> (Vec<Option<u64>>, Vec<(u64, TextRasterRequest)>) {
    fn walk<'a>(
        node: &'a EvaluatedNode,
        index: &mut usize,
        out: &mut Vec<(usize, Option<&'a EvaluatedText>)>,
    ) {
        let at = *index;
        *index += 1;
        match &node.content {
            NodeContent::Text(text) => {
                out.push((at, Some(text).filter(|text| !text.content.is_empty())));
            }
            NodeContent::Children(children) => {
                for child in children {
                    walk(child, index, out);
                }
            }
            NodeContent::Empty | NodeContent::Shape(_) => {}
        }
    }
    let unit = unit_of(size);
    let mut found = Vec::new();
    walk(&layout.root, &mut 0, &mut found);
    let mut keys = Vec::with_capacity(found.len());
    let mut requests: Vec<(u64, TextRasterRequest)> = Vec::new();
    for (node, text) in found {
        let Some(text) = text else {
            keys.push(None);
            continue;
        };
        let width = resolved
            .nodes
            .get(node)
            .map_or(0.0, |rects| rects.text_width);
        let mut request = text_request(text, unit, wrap(width), 1);
        let key = crate::text::layout_text_key(&request);
        request.content_version = key;
        if !requests.iter().any(|(held, _)| *held == key) {
            requests.push((key, request));
        }
        keys.push(Some(key));
    }
    (keys, requests)
}

/// Where each node of a valid layout tree lies on a picture of `size`, at
/// picture time `time`, laid out with `rasterizer`'s fonts as a frame lays it
/// out. `canvas` is the project canvas, which the tree's pixel lengths are
/// pixels of (the picture's own size when they are measured at it). Rects are
/// before the clip's transform and before node visuals: layout boxes, not
/// where they are drawn. An invalid tree gives unspecified rects, but never
/// panics.
///
/// # Errors
/// Returns [`TextError::Raster`] when the text host refuses a text: no
/// fonts, or a size out of range.
#[allow(clippy::cast_precision_loss)]
pub fn measure_layout(
    rasterizer: &TextRasterizer,
    root: &LayoutNode,
    time: MediaTime,
    size: RenderSize,
    canvas: (u32, u32),
) -> Result<LayoutBounds, TextError> {
    let evaluated = LayoutPlan::new(root, canvas).evaluate(time);
    let shaped = shape_texts(&evaluated, size, |request| {
        rasterizer.shape(request).map(Arc::new)
    })?;
    let resolved = resolve(&evaluated, size, &shaped);
    let (width, height) = (size.width as f32, size.height as f32);
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
            })
            .collect(),
    })
}
