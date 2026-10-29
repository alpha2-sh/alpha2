//! Flexbox and grid resolution with taffy: the only file that names it, so
//! an upgrade touches this file and the goldens.

use super::{LAYOUT_WRAP_SLACK, NodeRects, ResolvedLayout, wrap};
use crate::PictureSize;
use alpha2_eval::{EvaluatedLayout, EvaluatedNode, NodeContent, NodeLayout, Sides};
use alpha2_model::{
    AlignContent, AlignItems, Display, FlexDirection, FlexWrap, GridAutoFlow, GridEdge,
    GridPlacement, JustifyContent, LayoutLength, Overflow, Position, RepeatCount, TrackBreadth,
    TrackComponent, TrackList, TrackSize,
};
use alpha2_text::ShapedText;
use std::sync::Arc;
use taffy::{
    AvailableSpace, Dimension, GridTemplateComponent, GridTemplateRepetition, LengthPercentage,
    LengthPercentageAuto, MaxTrackSizingFunction, MinTrackSizingFunction, NodeId, Point, Rect,
    RepetitionCount, Size, Style, TaffyTree, TrackSizingFunction,
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
#[must_use]
pub fn resolve(
    layout: &EvaluatedLayout,
    size: PictureSize,
    shaped: &[Option<Arc<ShapedText>>],
) -> ResolvedLayout {
    let (width, height) = (size.width() as f32, size.height() as f32);
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

// A plan holds no pixels (they became fractions); were one left, it would read as a fraction.

fn length_auto(value: Option<LayoutLength>, unit: f32) -> LengthPercentageAuto {
    match value {
        None => LengthPercentageAuto::auto(),
        Some(LayoutLength::Fraction(value) | LayoutLength::Px(value)) => {
            LengthPercentageAuto::length(value * unit)
        }
        Some(LayoutLength::Percent(value)) => LengthPercentageAuto::percent(value / 100.0),
    }
}

fn dimension(value: Option<LayoutLength>, unit: f32) -> Dimension {
    match value {
        None => Dimension::auto(),
        Some(LayoutLength::Fraction(value) | LayoutLength::Px(value)) => {
            Dimension::length(value * unit)
        }
        Some(LayoutLength::Percent(value)) => Dimension::percent(value / 100.0),
    }
}

fn min_breadth(breadth: TrackBreadth, unit: f32) -> MinTrackSizingFunction {
    match breadth {
        TrackBreadth::Length(LayoutLength::Fraction(value) | LayoutLength::Px(value)) => {
            MinTrackSizingFunction::length(value * unit)
        }
        TrackBreadth::Length(LayoutLength::Percent(value)) => {
            MinTrackSizingFunction::percent(value / 100.0)
        }
        // A minimum is never fr: the parser refuses it, as CSS does.
        TrackBreadth::Auto | TrackBreadth::Fr(_) => MinTrackSizingFunction::auto(),
        TrackBreadth::MinContent => MinTrackSizingFunction::min_content(),
        TrackBreadth::MaxContent => MinTrackSizingFunction::max_content(),
    }
}

fn max_breadth(breadth: TrackBreadth, unit: f32) -> MaxTrackSizingFunction {
    match breadth {
        TrackBreadth::Length(LayoutLength::Fraction(value) | LayoutLength::Px(value)) => {
            MaxTrackSizingFunction::length(value * unit)
        }
        TrackBreadth::Length(LayoutLength::Percent(value)) => {
            MaxTrackSizingFunction::percent(value / 100.0)
        }
        TrackBreadth::Fr(share) => MaxTrackSizingFunction::fr(share),
        TrackBreadth::Auto => MaxTrackSizingFunction::auto(),
        TrackBreadth::MinContent => MaxTrackSizingFunction::min_content(),
        TrackBreadth::MaxContent => MaxTrackSizingFunction::max_content(),
    }
}

/// A track's sizing functions, as CSS reads a single breadth: a length is
/// fixed, `<n>fr` is `minmax(auto, <n>fr)`, a keyword sizes by content.
fn track(size: TrackSize, unit: f32) -> TrackSizingFunction {
    let (min, max) = match size {
        TrackSize::Breadth(TrackBreadth::Fr(share)) => (
            MinTrackSizingFunction::auto(),
            MaxTrackSizingFunction::fr(share),
        ),
        TrackSize::Breadth(breadth) => (min_breadth(breadth, unit), max_breadth(breadth, unit)),
        TrackSize::MinMax(min, max) => (min_breadth(min, unit), max_breadth(max, unit)),
        TrackSize::FitContent(limit) => (
            MinTrackSizingFunction::auto(),
            match limit {
                LayoutLength::Fraction(value) | LayoutLength::Px(value) => {
                    MaxTrackSizingFunction::fit_content_px(value * unit)
                }
                LayoutLength::Percent(value) => {
                    MaxTrackSizingFunction::fit_content_percent(value / 100.0)
                }
            },
        ),
    };
    TrackSizingFunction { min, max }
}

/// An explicit track list, `repeat()` kept for taffy to expand, with no line
/// names (the model refuses them).
fn template<S: taffy::CheapCloneStr>(
    tracks: Option<&TrackList>,
    unit: f32,
) -> Vec<GridTemplateComponent<S>> {
    tracks.map_or_else(Vec::new, |tracks| {
        tracks
            .0
            .iter()
            .map(|component| match component {
                TrackComponent::Single(size) => GridTemplateComponent::Single(track(*size, unit)),
                TrackComponent::Repeat(count, sizes) => {
                    GridTemplateComponent::Repeat(GridTemplateRepetition {
                        count: match count {
                            RepeatCount::Count(count) => RepetitionCount::Count(*count),
                            RepeatCount::AutoFill => RepetitionCount::AutoFill,
                            RepeatCount::AutoFit => RepetitionCount::AutoFit,
                        },
                        tracks: sizes.iter().map(|size| track(*size, unit)).collect(),
                        line_names: Vec::new(),
                    })
                }
            })
            .collect()
    })
}

/// Implicit track sizes: the model refuses `repeat()` in them.
fn implicit(tracks: Option<&TrackList>, unit: f32) -> Vec<TrackSizingFunction> {
    tracks.map_or_else(Vec::new, |tracks| {
        tracks.sizes().map(|size| track(size, unit)).collect()
    })
}

fn placement<S: taffy::CheapCloneStr>(
    placement: GridPlacement,
) -> taffy::Line<taffy::GridPlacement<S>> {
    let edge = |edge: GridEdge| match edge {
        GridEdge::Auto => taffy::GridPlacement::Auto,
        GridEdge::Line(line) => taffy::style_helpers::line(line),
        GridEdge::Span(count) => taffy::GridPlacement::Span(count),
    };
    taffy::Line {
        start: edge(placement.start),
        end: edge(placement.end),
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

const fn align_content(value: AlignContent) -> taffy::AlignContent {
    match value {
        AlignContent::Start => taffy::AlignContent::START,
        AlignContent::Center => taffy::AlignContent::CENTER,
        AlignContent::End => taffy::AlignContent::END,
        AlignContent::SpaceBetween => taffy::AlignContent::SPACE_BETWEEN,
        AlignContent::SpaceAround => taffy::AlignContent::SPACE_AROUND,
        AlignContent::SpaceEvenly => taffy::AlignContent::SPACE_EVENLY,
        AlignContent::Stretch => taffy::AlignContent::STRETCH,
    }
}

/// The taffy style of one node, lengths scaled by `unit` pixels. The root
/// takes the picture's size.
fn taffy_style(node: &NodeLayout, unit: f32, root_size: Option<Size<f32>>) -> Style {
    let insets = |insets: Sides| Rect {
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
        display: match node.display {
            Display::Flex => taffy::Display::Flex,
            Display::Grid => taffy::Display::Grid,
        },
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
            width: length(node.column_gap, unit),
            height: length(node.row_gap, unit),
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
        align_content: align_content(node.align_content),
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
        ..grid_style(node, unit)
    }
}

/// The grid container and grid item fields of one node's taffy style, lengths scaled by `unit`
/// pixels; taffy reads them only where a grid lays the node or its children out.
fn grid_style(node: &NodeLayout, unit: f32) -> Style {
    Style {
        justify_items: align(node.justify_items),
        justify_self: node.justify_self.map(align),
        grid_template_columns: template(node.grid_template_columns.as_ref(), unit),
        grid_template_rows: template(node.grid_template_rows.as_ref(), unit),
        grid_auto_columns: implicit(node.grid_auto_columns.as_ref(), unit),
        grid_auto_rows: implicit(node.grid_auto_rows.as_ref(), unit),
        grid_auto_flow: match node.grid_auto_flow {
            GridAutoFlow::Row => taffy::GridAutoFlow::Row,
            GridAutoFlow::Column => taffy::GridAutoFlow::Column,
            GridAutoFlow::RowDense => taffy::GridAutoFlow::RowDense,
            GridAutoFlow::ColumnDense => taffy::GridAutoFlow::ColumnDense,
        },
        grid_column: placement(node.grid_column),
        grid_row: placement(node.grid_row),
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
    // Compared with the text's own height, unrounded like its width: taffy
    // rounds edges by absolute position, so a whole-pixel box can be up to a
    // pixel shorter than the height it was given.
    let text_height = (unrounded.size.height
        - unrounded.border.top
        - unrounded.border.bottom
        - unrounded.padding.top
        - unrounded.padding.bottom)
        .max(0.0);
    let slot = out.len();
    out.push(NodeRects {
        border,
        padding,
        content,
        text_width,
        overflow: false,
        line_count: None,
        baselines: None,
    });
    let mut overflow = false;
    match &node.content {
        NodeContent::Text(_) => {
            let text = shaped.get(*texts).and_then(Option::as_deref);
            *texts += 1;
            let lines = text.map(|text| {
                let lines = text.lines_at(wrap(text_width));
                overflow = lines.height > text_height + LAYOUT_WRAP_SLACK
                    || text.min_content_width() > wrap(text_width);
                lines
            });
            out[slot].line_count = Some(lines.map_or(0, |lines| lines.line_count));
            out[slot].baselines = lines.map(|lines| {
                [
                    content[1] + lines.first_baseline,
                    content[1] + lines.last_baseline,
                ]
            });
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
