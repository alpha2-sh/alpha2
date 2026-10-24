//! Flexbox resolution with taffy: the only file that names it, so an upgrade
//! touches this file and the goldens.

use super::{NodeRects, ResolvedLayout, wrap};
use alpha2_eval::{EvaluatedLayout, EvaluatedNode, NodeContent, NodeLayout};
use alpha2_model::{
    AlignItems, FlexDirection, FlexWrap, JustifyContent, LayoutLength, Overflow, Position,
};
use alpha2_render::RenderSize;
use alpha2_text::ShapedText;
use std::sync::Arc;
use taffy::{
    AvailableSpace, Dimension, LengthPercentage, LengthPercentageAuto, NodeId, Point, Rect, Size,
    Style, TaffyTree,
};

/// What a leaf measures by.
#[derive(Copy, Clone)]
enum NodeContext {
    /// The text node with this index among the shaped texts.
    Text(usize),
    /// A shape node, an empty node, or an empty text: no size of its own.
    Other,
}

/// Lays `layout` out on a picture of `size` pixels. `shaped` holds each
/// text node's shaped text, in preorder, `None` for an empty text.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn resolve(
    layout: &EvaluatedLayout,
    size: RenderSize,
    shaped: &[Option<Arc<ShapedText>>],
) -> ResolvedLayout {
    let (width, height) = (size.width as f32, size.height as f32);
    let unit = width.min(height);
    let mut tree = TaffyTree::<NodeContext>::new();
    let mut ids = Vec::new();
    let mut texts = 0;
    let root = build(
        &mut tree,
        &layout.root,
        unit,
        Some(Size { width, height }),
        &mut ids,
        &mut texts,
    );
    // A tree built from a valid layout always lays out; a failure leaves every rect empty.
    let laid_out = tree
        .compute_layout_with_measure(
            root,
            Size {
                width: AvailableSpace::Definite(width),
                height: AvailableSpace::Definite(height),
            },
            |inputs, _, context, style| match context {
                Some(NodeContext::Text(index)) => {
                    let text = shaped.get(*index).and_then(Option::as_deref);
                    taffy::compute_leaf_layout(
                        inputs,
                        style,
                        |_, _| 0.0,
                        |known, available| {
                            let Some(text) = text else {
                                return Size::ZERO;
                            };
                            let (min, max) = (text.min_content_width(), text.max_content_width());
                            let width = known.width.unwrap_or(match available.width {
                                AvailableSpace::MinContent => min,
                                AvailableSpace::MaxContent => max,
                                AvailableSpace::Definite(space) => space.min(max).max(min),
                            });
                            let height = known
                                .height
                                .unwrap_or_else(|| text.lines_at(wrap(width)).height);
                            Size { width, height }
                        },
                    )
                }
                _ => taffy::compute_leaf_layout(
                    inputs,
                    style,
                    |_, _| 0.0,
                    |known, _| Size {
                        width: known.width.unwrap_or(0.0),
                        height: known.height.unwrap_or(0.0),
                    },
                ),
            },
        )
        .is_ok();
    let mut nodes = Vec::with_capacity(ids.len());
    if laid_out {
        collect(
            &tree,
            &layout.root,
            &ids,
            [0.0, 0.0],
            &mut 0,
            shaped,
            &mut 0,
            &mut nodes,
        );
    } else {
        nodes.resize(ids.len(), NodeRects::default());
    }
    ResolvedLayout { nodes }
}

/// Adds `node` and its subtree to `tree`, children first; `ids` gets each
/// node's id in preorder.
fn build(
    tree: &mut TaffyTree<NodeContext>,
    node: &EvaluatedNode,
    unit: f32,
    root_size: Option<Size<f32>>,
    ids: &mut Vec<NodeId>,
    texts: &mut usize,
) -> NodeId {
    let style = taffy_style(&node.layout, unit, root_size);
    let slot = ids.len();
    ids.push(NodeId::from(0_u64));
    let id = match &node.content {
        NodeContent::Children(children) => {
            let children = children
                .iter()
                .map(|child| build(tree, child, unit, None, ids, texts))
                .collect::<Vec<_>>();
            tree.new_with_children(style, &children)
        }
        NodeContent::Text(_) => {
            let index = *texts;
            *texts += 1;
            tree.new_leaf_with_context(style, NodeContext::Text(index))
        }
        NodeContent::Empty | NodeContent::Shape(_) => {
            tree.new_leaf_with_context(style, NodeContext::Other)
        }
    }
    .expect("taffy adds nodes without failing");
    ids[slot] = id;
    id
}

fn length(value: f32, unit: f32) -> LengthPercentage {
    LengthPercentage::length(value * unit)
}

fn length_auto(value: Option<LayoutLength>, unit: f32) -> LengthPercentageAuto {
    match value {
        None => LengthPercentageAuto::auto(),
        Some(LayoutLength::Length { value }) => LengthPercentageAuto::length(value * unit),
        Some(LayoutLength::Percent { value }) => LengthPercentageAuto::percent(value / 100.0),
    }
}

fn dimension(value: Option<LayoutLength>, unit: f32) -> Dimension {
    match value {
        None => Dimension::auto(),
        Some(LayoutLength::Length { value }) => Dimension::length(value * unit),
        Some(LayoutLength::Percent { value }) => Dimension::percent(value / 100.0),
    }
}

const fn align(value: AlignItems) -> taffy::AlignItems {
    match value {
        AlignItems::Start => taffy::AlignItems::START,
        AlignItems::Center => taffy::AlignItems::CENTER,
        AlignItems::End => taffy::AlignItems::END,
        AlignItems::Stretch => taffy::AlignItems::STRETCH,
    }
}

/// The taffy style of one node, lengths scaled by `unit` pixels. The root
/// takes the picture's size.
fn taffy_style(node: &NodeLayout, unit: f32, root_size: Option<Size<f32>>) -> Style {
    let gap = length(node.gap, unit);
    let insets = |insets: alpha2_model::Insets| Rect {
        left: length(insets.left, unit),
        right: length(insets.right, unit),
        top: length(insets.top, unit),
        bottom: length(insets.bottom, unit),
    };
    let margin = Rect {
        left: LengthPercentageAuto::length(node.margin.left * unit),
        right: LengthPercentageAuto::length(node.margin.right * unit),
        top: LengthPercentageAuto::length(node.margin.top * unit),
        bottom: LengthPercentageAuto::length(node.margin.bottom * unit),
    };
    let border = length(node.border_width, unit);
    let [left, top, right, bottom] = node.inset;
    let overflow = match node.overflow {
        Overflow::Visible => taffy::Overflow::Visible,
        Overflow::Hidden => taffy::Overflow::Hidden,
    };
    Style {
        display: taffy::Display::Flex,
        box_sizing: taffy::BoxSizing::BorderBox,
        direction: taffy::Direction::Ltr,
        flex_direction: match node.flex_direction {
            FlexDirection::Row => taffy::FlexDirection::Row,
            FlexDirection::Column => taffy::FlexDirection::Column,
            FlexDirection::RowReverse => taffy::FlexDirection::RowReverse,
            FlexDirection::ColumnReverse => taffy::FlexDirection::ColumnReverse,
        },
        flex_wrap: match node.flex_wrap {
            FlexWrap::NoWrap => taffy::FlexWrap::NoWrap,
            FlexWrap::Wrap => taffy::FlexWrap::Wrap,
        },
        gap: Size {
            width: gap,
            height: gap,
        },
        justify_content: match node.justify_content {
            JustifyContent::Start => taffy::JustifyContent::START,
            JustifyContent::Center => taffy::JustifyContent::CENTER,
            JustifyContent::End => taffy::JustifyContent::END,
            JustifyContent::SpaceBetween => taffy::JustifyContent::SPACE_BETWEEN,
            JustifyContent::SpaceAround => taffy::JustifyContent::SPACE_AROUND,
            JustifyContent::SpaceEvenly => taffy::JustifyContent::SPACE_EVENLY,
        },
        align_items: align(node.align_items),
        align_self: node.align_self.map(align),
        align_content: taffy::AlignContent::STRETCH,
        padding: insets(node.padding),
        margin,
        border: Rect {
            left: border,
            right: border,
            top: border,
            bottom: border,
        },
        size: root_size.map_or_else(
            || Size {
                width: dimension(node.width, unit),
                height: dimension(node.height, unit),
            },
            |size| Size {
                width: Dimension::length(size.width),
                height: Dimension::length(size.height),
            },
        ),
        min_size: Size {
            width: length_auto(node.min_width, unit),
            height: length_auto(node.min_height, unit),
        },
        max_size: Size {
            width: length_auto(node.max_width, unit),
            height: length_auto(node.max_height, unit),
        },
        aspect_ratio: node.aspect_ratio,
        flex_grow: node.flex_grow,
        flex_shrink: node.flex_shrink,
        flex_basis: dimension(node.flex_basis, unit),
        // Every box is positioned, so an absolute box's containing block is its parent.
        position: match node.position {
            Position::Relative => taffy::Position::Relative,
            Position::Absolute => taffy::Position::Absolute,
        },
        inset: Rect {
            left: length_auto(left, unit),
            top: length_auto(top, unit),
            right: length_auto(right, unit),
            bottom: length_auto(bottom, unit),
        },
        overflow: Point {
            x: overflow,
            y: overflow,
        },
        scrollbar_width: 0.0,
        ..Style::default()
    }
}

/// Reads each node's rects in preorder: absolute whole-pixel boxes, summed
/// from the rounded locations, which taffy rounds by absolute position.
#[allow(clippy::too_many_arguments)]
fn collect(
    tree: &TaffyTree<NodeContext>,
    node: &EvaluatedNode,
    ids: &[NodeId],
    origin: [f32; 2],
    index: &mut usize,
    shaped: &[Option<Arc<ShapedText>>],
    texts: &mut usize,
    out: &mut Vec<NodeRects>,
) {
    let id = ids[*index];
    *index += 1;
    let rounded = tree.layout(id).expect("laid out");
    let unrounded = tree.unrounded_layout(id);
    let left = origin[0] + rounded.location.x;
    let top = origin[1] + rounded.location.y;
    let border = [
        left,
        top,
        left + rounded.size.width,
        top + rounded.size.height,
    ];
    let padding = [
        border[0] + rounded.border.left,
        border[1] + rounded.border.top,
        border[2] - rounded.border.right,
        border[3] - rounded.border.bottom,
    ];
    let content = [
        padding[0] + rounded.padding.left,
        padding[1] + rounded.padding.top,
        padding[2] - rounded.padding.right,
        padding[3] - rounded.padding.bottom,
    ];
    let text_width = (unrounded.size.width
        - unrounded.border.left
        - unrounded.border.right
        - unrounded.padding.left
        - unrounded.padding.right)
        .max(0.0);
    let slot = out.len();
    out.push(NodeRects {
        border,
        padding,
        content,
        text_width,
        overflow: false,
        line_count: None,
    });
    let mut overflow = false;
    match &node.content {
        NodeContent::Text(_) => {
            let text = shaped.get(*texts).and_then(Option::as_deref);
            *texts += 1;
            let lines = text.map(|text| {
                let lines = text.lines_at(wrap(text_width));
                overflow = lines.height > content[3] - content[1] + 0.5
                    || text.min_content_width() > wrap(text_width);
                lines.line_count
            });
            out[slot].line_count = Some(lines.unwrap_or(0));
        }
        NodeContent::Children(children) => {
            for child in children {
                let at = out.len();
                collect(tree, child, ids, [left, top], index, shaped, texts, out);
                if child.layout.position == Position::Relative {
                    let [l, t, r, b] = out[at].border;
                    overflow |= l < padding[0] - 0.5
                        || t < padding[1] - 0.5
                        || r > padding[2] + 0.5
                        || b > padding[3] + 0.5;
                }
            }
        }
        NodeContent::Empty | NodeContent::Shape(_) => {}
    }
    out[slot].overflow = overflow;
}
