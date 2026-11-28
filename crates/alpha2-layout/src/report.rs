//! Where a text's lines, words and clusters lie: the static layout the text host draws.

use std::ops::Range;

use alpha2_model::TextStyle;
use alpha2_text::{TextRasterError, TextRasterizer};

use crate::{CanvasRect, PictureSize, TextAt};

/// A text's layout on a canvas, rectangles in fractions of the canvas, before the clip's
/// transform: no animator states, no tracking or pushing, at the fitted size and after the
/// clamp.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLayoutReport {
    /// The base size drawn, a fraction of the canvas's shorter side.
    pub font_size: f32,
    /// The layout box, as [`crate::TextBounds::layout`].
    pub layout: CanvasRect,
    /// Lines, top to bottom.
    pub lines: Vec<LineReport>,
    /// UAX #29 words holding a letter or digit, in order.
    pub words: Vec<WordReport>,
    /// Grapheme clusters in logical order.
    pub clusters: Vec<ClusterReport>,
}

/// One line of a [`TextLayoutReport`].
#[derive(Clone, Debug, PartialEq)]
pub struct LineReport {
    /// Its clusters, as indices into [`TextLayoutReport::clusters`].
    pub clusters: Range<usize>,
    /// Its line box: the line height times its largest size tall, its advance (without hanging
    /// spaces) wide.
    pub line_box: CanvasRect,
    /// The em box of its largest size over the same advance.
    pub em: CanvasRect,
    /// Its baseline, a fraction of the canvas height.
    pub baseline: f32,
}

/// One word of a [`TextLayoutReport`].
#[derive(Clone, Debug, PartialEq)]
pub struct WordReport {
    /// Its clusters, as indices into [`TextLayoutReport::clusters`].
    pub clusters: Range<usize>,
    /// Its advance by its em box, one rectangle per line it lies on.
    pub boxes: Vec<CanvasRect>,
    /// The union of its clusters' ink, `None` when none draws.
    pub ink: Option<CanvasRect>,
}

/// One cluster of a [`TextLayoutReport`].
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterReport {
    /// Its UTF-8 byte range in the text; the ellipsis has an empty one, at the cut.
    pub bytes: Range<usize>,
    /// The run holding its first byte, `None` for a plain string.
    pub run: Option<usize>,
    /// Its word, `None` for spaces, punctuation and the ellipsis.
    pub word: Option<u32>,
    /// Its line.
    pub line: u32,
    /// Whether it holds only white space.
    pub space: bool,
    /// Whether it is the ellipsis of a clamped text.
    pub ellipsis: bool,
    /// Its size, a fraction of the canvas's shorter side.
    pub font_size: f32,
    /// Its line's baseline, a fraction of the canvas height.
    pub baseline: f32,
    /// Its advance by its own em box.
    pub cluster_box: CanvasRect,
    /// The bounds of what it draws, strokes included, at most a pixel loose; `None` when it
    /// draws nothing.
    pub ink: Option<CanvasRect>,
}

/// The layout of `text` in `style` on a canvas of `size`, laid out with `rasterizer`'s fonts
/// and placed as a frame places it.
///
/// # Errors
/// Returns the text host's refusal: no fonts, or a style out of range.
pub fn text_layout(
    rasterizer: &TextRasterizer,
    text: &TextAt<'_>,
    style: &TextStyle,
    size: PictureSize,
) -> Result<TextLayoutReport, TextRasterError> {
    let placed = crate::text::place(rasterizer, text, style, size)?;
    let layout = rasterizer.layout_with_ink(&placed.request)?;
    let unit = size.unit();
    let rect = |bounds: [f32; 4]| placed.rect(bounds);
    let baseline = |pixels: f32| (placed.origin[1] + pixels) / placed.canvas[1];
    let em = |baseline: f32, size: f32| [baseline - 0.8 * size, baseline + 0.2 * size];
    let run_ends = text
        .content
        .runs()
        .iter()
        .scan(0, |end, run| {
            *end += run.text.len();
            Some(*end)
        })
        .collect::<Vec<_>>();
    let clusters = layout
        .clusters
        .iter()
        .map(|cluster| {
            let [top, bottom] = em(cluster.baseline, cluster.font_size);
            ClusterReport {
                bytes: cluster.bytes.clone(),
                run: (!run_ends.is_empty()).then(|| {
                    run_ends
                        .partition_point(|end| *end <= cluster.bytes.start)
                        .min(run_ends.len() - 1)
                }),
                word: cluster.word,
                line: cluster.line,
                space: cluster.is_space,
                ellipsis: cluster.ellipsis,
                font_size: cluster.font_size / unit,
                baseline: baseline(cluster.baseline),
                cluster_box: rect([cluster.x, top, cluster.x + cluster.advance, bottom]),
                ink: cluster.ink.map(rect),
            }
        })
        .collect::<Vec<_>>();
    let lines = layout
        .lines
        .iter()
        .map(|line| {
            let [em_top, em_bottom] = em(line.baseline, line.size);
            let (left, right) = (line.left, line.left + line.width);
            LineReport {
                clusters: line.clusters.clone(),
                line_box: rect([
                    left,
                    line.box_top,
                    right,
                    line.box_top + placed.request.line_height * line.size,
                ]),
                em: rect([left, em_top, right, em_bottom]),
                baseline: baseline(line.baseline),
            }
        })
        .collect();
    let words = words_of(&layout, &clusters, &rect);
    Ok(TextLayoutReport {
        font_size: placed.font_size,
        layout: placed.layout,
        lines,
        words,
        clusters,
    })
}

/// The words of `layout`, their boxes per line by `rect`, their ink from `clusters`.
fn words_of(
    layout: &alpha2_text::TextLayout,
    clusters: &[ClusterReport],
    rect: &impl Fn([f32; 4]) -> CanvasRect,
) -> Vec<WordReport> {
    let em = |baseline: f32, size: f32| [baseline - 0.8 * size, baseline + 0.2 * size];
    let mut words = Vec::<WordReport>::new();
    for (index, cluster) in layout.clusters.iter().enumerate() {
        let Some(word) = cluster.word else {
            continue;
        };
        let word = usize::try_from(word).unwrap_or(usize::MAX);
        while words.len() <= word {
            words.push(WordReport {
                clusters: index..index,
                boxes: Vec::new(),
                ink: None,
            });
        }
        let entry = &mut words[word];
        if entry.clusters.is_empty() {
            entry.clusters.start = index;
        }
        entry.clusters.end = index + 1;
    }
    for word in &mut words {
        let members = &layout.clusters[word.clusters.clone()];
        let mut line_of = None;
        let mut fragment: Option<[f32; 4]> = None;
        let mut fragments = Vec::new();
        for cluster in members.iter().filter(|cluster| cluster.word.is_some()) {
            let [top, bottom] = em(cluster.baseline, cluster.font_size);
            let piece = [cluster.x, top, cluster.x + cluster.advance, bottom];
            if line_of != Some(cluster.line) {
                fragments.extend(fragment.take());
                line_of = Some(cluster.line);
            }
            fragment = Some(fragment.map_or(piece, |held| {
                [
                    held[0].min(piece[0]),
                    held[1].min(piece[1]),
                    held[2].max(piece[2]),
                    held[3].max(piece[3]),
                ]
            }));
        }
        fragments.extend(fragment);
        word.boxes = fragments.into_iter().map(rect).collect();
        word.ink = clusters[word.clusters.clone()]
            .iter()
            .filter_map(|cluster| cluster.ink)
            .reduce(|held, ink| CanvasRect {
                left: held.left.min(ink.left),
                top: held.top.min(ink.top),
                right: held.right.max(ink.right),
                bottom: held.bottom.max(ink.bottom),
            });
    }
    words
}
