//! Text shaping and rasterization for alpha2 hosts.
//!
//! Produces the engine's [`TextTexture`] in pure Rust from caller-supplied fonts. Paragraphs are
//! split by direction (UAX #9), script and font, shaped with rustybuzz, broken per UAX #14 and
//! rasterized with `ab_glyph_rasterizer` (color glyphs in `color`). The same request and fonts
//! give the same pixels natively and in wasm, so browser preview and native export match.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use ab_glyph_rasterizer::{Point, Rasterizer, point};
use alpha2_media::{
    EmojiPolicy, HostTexture, HostTexturePixels, Premultiplication, RasterColorSpace, TextAlign,
    TextTexture,
};
use rustybuzz::ttf_parser::{self, GlyphId, OutlineBuilder};
use rustybuzz::{Direction, UnicodeBuffer};
use unicode_bidi::ParagraphBidiInfo;
use unicode_linebreak::BreakOpportunity;
use unicode_script::{Script, UnicodeScript};
use unicode_segmentation::UnicodeSegmentation;

mod animated;
mod boxes;
mod color;
mod font_check;
mod gradient;
mod highlight;
mod paint;
mod runs;
mod unicode_tables;

pub use animated::{
    ClusterGrouping, ClusterInfo, ClusterState, ColorMix, LineInfo, MAX_CLUSTER_SCALE,
    MAX_REVEAL_EDGES, RevealEdge, TextLayout,
};
pub use gradient::{TextGradient, TextGradientKind, TextGradientStop, TextSpread};

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_FONT_SIZE: f32 = 2_048.0;
const MAX_LINE_HEIGHT: f32 = 10.0;
/// Combined stroke width limit, in pixels.
const MAX_STROKE_EXTENT: f32 = 64.0;
const MAX_STROKES: usize = 8;
/// Most spans one request holds.
pub const MAX_TEXT_SPANS: usize = 512;
const MAX_BACKGROUND_PADDING: f32 = 256.0;
/// The widest wrapping width and the largest raster side in pixels.
pub const MAX_TEXT_BOX: u32 = 16_384;
#[allow(clippy::cast_precision_loss)]
const MAX_TEXT_BOX_PIXELS: f32 = MAX_TEXT_BOX as f32;
/// In font sizes.
const LETTER_SPACING_RANGE: std::ops::RangeInclusive<f32> = -0.5..=1.0;
/// Bytes the planes of one raster may take together.
const MAX_RASTER_BYTES: u64 = 1 << 30;

/// The pixel count of a `width` x `height` raster whose planes hold `bytes_per_pixel` bytes per
/// pixel together, or [`TextRasterError::TooLarge`] past the largest frame
/// ([`alpha2_media::MAX_FRAME_PIXELS`]) or [`MAX_RASTER_BYTES`]. Checked before any plane exists.
fn raster_pixels(width: u32, height: u32, bytes_per_pixel: u64) -> Result<usize, TextRasterError> {
    let pixels = u64::from(width) * u64::from(height);
    if width > MAX_TEXT_BOX
        || height > MAX_TEXT_BOX
        || pixels > alpha2_media::MAX_FRAME_PIXELS
        || pixels.saturating_mul(bytes_per_pixel) > MAX_RASTER_BYTES
    {
        return Err(TextRasterError::TooLarge);
    }
    usize::try_from(pixels).map_err(|_| TextRasterError::TooLarge)
}

/// Bytes per pixel of a raster's planes: coverage (1), RGBA8 result (4), split background (4),
/// color glyph RGBA (4) with its stroke ink plane (1), and one band per stroke (1 each).
fn raster_bytes_per_pixel(request: &TextRasterRequest, strokes: usize, colors: bool) -> u64 {
    let split = request.split_background
        && (request.background.is_some()
            || request
                .spans
                .iter()
                .any(|span| span.background_color.is_some()));
    let strokes = strokes as u64;
    1 + 4
        + if split { 4 } else { 0 }
        + if colors {
            4 + u64::from(strokes > 0)
        } else {
            0
        }
        + strokes
}

/// One caller-owned font registered in fallback order.
#[derive(Clone, Debug)]
pub struct TextFont {
    /// Stable family name used in cache keys and diagnostics.
    pub family: String,
    /// OpenType font bytes.
    pub bytes: Arc<[u8]>,
    /// Face index for font collections.
    pub face_index: u32,
}

/// One text layout request.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRasterRequest {
    /// UTF-8 text shaped by the selected font fallback stack.
    pub text: String,
    /// Logical font size in pixels.
    pub font_size: f32,
    /// Maximum layout width. Non-positive values disable wrapping.
    pub max_width: f32,
    /// Baseline-to-baseline distance in font sizes. The em box (0.8 above the baseline, 0.2
    /// below) is centered in lines taller than an em.
    pub line_height: f32,
    /// Per-line horizontal alignment.
    pub align: TextAlign,
    /// Glyph paint. Color glyphs keep their own colors except for text-color parts, which use
    /// [`TextFill::foreground`], whose alpha is also their opacity.
    pub fill: TextFill,
    /// Whether color glyphs are drawn in color, and which fonts emoji take.
    pub emoji_policy: EmojiPolicy,
    /// Caller-managed version. Change it whenever content/style/font inputs change.
    pub content_version: u64,
    /// Outlines, innermost first, each drawn behind the previous. At most 8, reaching at most 64
    /// pixels together.
    pub strokes: Vec<TextStroke>,
    /// A box behind each line, drawn behind the strokes.
    pub background: Option<TextBackground>,
    /// Draws the background into [`TextRasterResult::background`] instead of under the glyphs,
    /// so a caller can draw something (a shadow) between them.
    pub split_background: bool,
    /// Preferred family, matched against any [`font_family_names`] entry after NFC and
    /// lowercasing; the fallback stack fills missing glyphs. An unknown family keeps the fallback
    /// order and is listed by [`TextRasterizer::unmatched_families`].
    pub font_family: Option<String>,
    /// Space after each grapheme cluster, like CSS `letter-spacing`, in `[-0.5, 1]` font sizes.
    /// Not applied after a line's last cluster or within cursive scripts. Non-zero values disable
    /// optional ligatures.
    pub letter_spacing: f32,
    /// Weight in `[1, 1000]`, matched per CSS Fonts 4 and applied to a variable face's `wght`
    /// axis. Faces are never synthetically emboldened.
    pub font_weight: u16,
    /// Where lines are cut, as CSS `line-clamp` cuts them; `None` keeps every line.
    pub clamp: Option<TextClamp>,
    /// Byte ranges of `text` drawn with their own values, as CSS inline boxes: in order, without
    /// overlap, on character boundaries, at most [`MAX_TEXT_SPANS`]. Text outside every span
    /// takes the request's values; an empty list draws as the request alone.
    pub spans: Vec<TextSpan>,
    /// The most an animation can widen a cluster's advance ([`ClusterState::advance_scale`]),
    /// in `[1, MAX_CLUSTER_SCALE]`: lines wrap at `max_width` over it and align in `max_width`, so
    /// a line pushed apart that far still fits. 1 wraps at `max_width`.
    pub advance_headroom: f32,
    /// The units whose clusters an animated raster scales and turns about one pivot.
    pub animator_grouping: ClusterGrouping,
    /// A box over the run being said, drawn with the background boxes (and into
    /// [`TextRasterResult::background`] when split).
    pub highlight: Option<TextHighlightRequest>,
    /// Where in each group's box (its clusters' advances by their em boxes) the pivot lies,
    /// fractions from its left and top in `[0, 1]`; `[0.5, 0.5]` is its centre.
    pub animator_pivot: [f32; 2],
}

impl TextRasterRequest {
    /// The width lines break at: `max_width` over [`Self::advance_headroom`], non-positive for
    /// none.
    #[must_use]
    pub fn wrap_width(&self) -> f32 {
        wrap_width(self.max_width, self.advance_headroom)
    }
}

/// The width lines of a text `max_width` wide (non-positive for any width) break at when
/// animation can widen its advances by `advance_headroom`
/// ([`TextRasterRequest::advance_headroom`]).
#[must_use]
pub fn wrap_width(max_width: f32, advance_headroom: f32) -> f32 {
    if max_width > 0.0 && advance_headroom > 1.0 {
        max_width / advance_headroom
    } else {
        max_width
    }
}

/// Which faces of a family a span prefers, as CSS `font-style`.
#[derive(Copy, Clone, Debug, Default, Eq, Hash, PartialEq)]
pub enum FontStyle {
    /// Upright faces, then oblique, then italic.
    #[default]
    Normal,
    /// Italic faces, then oblique, then upright ones sheared.
    Italic,
    /// Oblique faces, then italic, then upright ones sheared.
    Oblique,
}

/// A byte range of a request's text with its own face, size, spacing, paint and decorations.
///
/// A change of family, weight, style, size or letter spacing shapes the span apart, with its
/// neighbours' text as shaping context; a change of paint or decoration only does not. A
/// grapheme cluster spanning two spans takes the values of the one holding its first byte.
#[derive(Clone, Debug, PartialEq)]
pub struct TextSpan {
    /// Byte range in [`TextRasterRequest::text`], on character boundaries.
    pub bytes: Range<usize>,
    /// Preferred family, as [`TextRasterRequest::font_family`].
    pub font_family: Option<String>,
    /// Weight in `[1, 1000]`, as [`TextRasterRequest::font_weight`].
    pub font_weight: u16,
    /// Face style; a face that is neither italic nor oblique is sheared when one is asked.
    pub font_style: FontStyle,
    /// Size in pixels. Each line is as tall as the largest size on it, at least the request's.
    pub font_size: f32,
    /// Space after each cluster in pixels, in `[-0.5, 1]` of `font_size`.
    pub letter_spacing: f32,
    /// Glyph paint; gradient positions are in the whole text's layout box.
    pub fill: TextFill,
    /// Bands around the span's glyphs, as [`TextRasterRequest::strokes`].
    pub strokes: Vec<TextStroke>,
    /// Straight sRGB RGBA of a box behind the span on each line, its advances wide and its em
    /// box tall.
    pub background_color: Option<[u8; 4]>,
    /// A line under the span, placed and sized by its first face.
    pub underline: bool,
    /// A line through the span, placed and sized by its first face.
    pub line_through: bool,
}

/// Most bytes in a [`TextClamp::ellipsis`].
pub const MAX_ELLIPSIS_BYTES: usize = 256;

/// Lines past the clamp point ([`line_limit`]) are discarded, as CSS `line-clamp` discards them.
/// A cut text's last line ends with the ellipsis, one more cluster ([`ClusterInfo::ellipsis`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TextClamp {
    /// Most lines, at least 1, as CSS `max-lines`; `None` sets no count.
    pub max_lines: Option<u32>,
    /// Height in pixels the kept lines' box fits in, as the automatic clamp point of CSS
    /// `max-lines: auto`; non-positive sets no height.
    pub max_height: f32,
    /// What ends a cut line, as CSS `block-ellipsis`, at most [`MAX_ELLIPSIS_BYTES`] bytes with
    /// no `\n`; `None` cuts without a mark.
    pub ellipsis: Option<String>,
}

/// The most lines a clamp keeps of text `font_size` pixels tall with lines `line_height` font
/// sizes apart: at most `max_lines`, and as many as fit in `max_height` pixels (non-positive
/// for no height), but at least one. `None` when neither limits.
#[must_use]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]
pub fn line_limit(
    font_size: f32,
    line_height: f32,
    max_lines: Option<u32>,
    max_height: f32,
) -> Option<u32> {
    let advance = font_size * line_height;
    let by_height = (max_height > 0.0).then(|| {
        // An estimate, then the exact comparison the box height makes.
        let first = advance.max(font_size);
        let estimate = ((max_height - first) / advance).floor() + 1.0;
        let mut lines = if estimate.is_finite() {
            estimate.clamp(1.0, 1.0e9) as u32
        } else {
            1
        };
        while lines > 1 && box_height(lines, advance, font_size) > max_height {
            lines -= 1;
        }
        while lines < 1_000_000_000 && box_height(lines + 1, advance, font_size) <= max_height {
            lines += 1;
        }
        lines
    });
    match (max_lines.map(|lines| lines.max(1)), by_height) {
        (Some(count), Some(height)) => Some(count.min(height)),
        (count, height) => count.or(height),
    }
}

/// What paints a text's glyphs.
#[derive(Clone, Debug, PartialEq)]
pub enum TextFill {
    /// Straight (not premultiplied) sRGB RGBA.
    Solid([u8; 4]),
    /// A gradient over the layout box.
    Gradient(TextGradient),
}

impl TextFill {
    /// Color used for a color glyph's text-color parts: the solid color or the first stop.
    #[must_use]
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn foreground(&self) -> [u8; 4] {
        match self {
            Self::Solid(color) => *color,
            Self::Gradient(gradient) => gradient.stops.first().map_or([0; 4], |stop| {
                stop.color
                    .map(|value| (value.clamp(0.0, 1.0) * 255.0).round() as u8)
            }),
        }
    }

    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Solid(_) => true,
            Self::Gradient(gradient) => gradient.is_valid(),
        }
    }
}

/// An outline band drawn behind the glyphs and earlier strokes.
///
/// Stroke `k` dilates glyph coverage by a disc whose radius is the sum of widths up to `k`, with
/// the disc's last pixel weighted down for antialiasing.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextStroke {
    /// Straight sRGB RGBA.
    pub color: [u8; 4],
    /// Positive width in pixels.
    pub width: f32,
}

/// A box behind each non-empty line: its advance by its em box, grown by `padding`, with a
/// radius and a border. Overlapping boxes show their union with one outline.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBackground {
    /// Straight sRGB RGBA.
    pub color: [u8; 4],
    /// Space between the text and the box edge in pixels, top first, clockwise, each in
    /// `[0, 256]`.
    pub padding: [f32; 4],
    /// Every corner's radius in pixels, capped at half the box's shorter side.
    pub radius: f32,
    /// A border inside the box's edge.
    pub border: Option<TextBoxBorder>,
    /// Which part of each line the box spans.
    pub extent: BoxExtent,
}

/// Which part of each line a text's background box spans.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum BoxExtent {
    /// From the line's first cluster to its last that is not a hanging space, where tracking
    /// and pushing place them.
    #[default]
    Line,
    /// From the first to the last revealed cluster that is not white space: one shown, not
    /// scaled to nothing, and not hidden by a reveal edge. A line with none has no box; a line
    /// whose clusters all show has its line box.
    Revealed,
    /// One box over the whole layout box: the wrap width by the lines' height.
    Block,
}

/// A box over the run being said that slides from run to run, drawn over the background boxes
/// and run backgrounds and under the glyphs.
#[derive(Clone, Debug, PartialEq)]
pub struct TextHighlightRequest {
    /// Its color, padding, radius and border; its extent is not read.
    pub look: TextBackground,
    /// Byte ranges of the text with their progress in `[0, 1]`, in the order their runs started,
    /// the first at 1, at most [`MAX_HIGHLIGHT_STEPS`]. The box starts as the first range's first
    /// line fragment (its clusters' advances, white space at its ends excluded, by its largest
    /// em box) and moves toward each next one's by that one's progress; the last range's other
    /// fragments are drawn whole.
    pub steps: Vec<(Range<usize>, f32)>,
}

/// Most steps a [`TextHighlightRequest`] takes.
pub const MAX_HIGHLIGHT_STEPS: usize = 512;

/// A text box's border.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBoxBorder {
    /// Width in pixels, positive.
    pub width: f32,
    /// Straight sRGB RGBA.
    pub color: [u8; 4],
    /// Dashes along the border's centre line; `None` draws it solid.
    pub dash: Option<TextBoxDash>,
}

/// SVG `stroke-dasharray` and `stroke-dashoffset` in pixels, from the end of the top-left
/// corner clockwise.
#[derive(Clone, Debug, PartialEq)]
pub struct TextBoxDash {
    /// Alternating dash and gap lengths, 1 to 16 non-negative entries, not all zero; an odd
    /// count repeats once more.
    pub pattern: Vec<f32>,
    /// How far into the pattern the border starts.
    pub offset: f32,
}

impl TextBackground {
    /// A box with the same padding on every side, no radius and no border.
    #[must_use]
    pub const fn padded(color: [u8; 4], padding: f32) -> Self {
        Self {
            color,
            padding: [padding; 4],
            radius: 0.0,
            border: None,
            extent: BoxExtent::Line,
        }
    }

    fn is_valid(&self) -> bool {
        let length = |value: f32| value.is_finite() && value >= 0.0;
        self.padding
            .iter()
            .all(|side| length(*side) && *side <= MAX_BACKGROUND_PADDING)
            && length(self.radius)
            && self.radius <= MAX_TEXT_BOX_PIXELS
            && self.border.as_ref().is_none_or(|border| {
                border.width.is_finite()
                    && border.width > 0.0
                    && border.width <= MAX_TEXT_BOX_PIXELS
                    && border.dash.as_ref().is_none_or(|dash| {
                        (1..=16).contains(&dash.pattern.len())
                            && dash
                                .pattern
                                .iter()
                                .all(|entry| length(*entry) && *entry <= MAX_TEXT_BOX_PIXELS)
                            && dash.pattern.iter().any(|entry| *entry > 0.0)
                            && dash.offset.is_finite()
                    })
            })
    }

    /// Whether the boxes are plain rectangles.
    pub(crate) fn is_plain(&self) -> bool {
        self.radius == 0.0 && self.border.is_none()
    }
}

/// Rasterized pixels and their text-specific layout descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRasterResult {
    /// Layout facts sharing the generic compositor texture contract.
    pub descriptor: TextTexture,
    /// Tightly packed premultiplied RGBA8 pixels.
    pub pixels: HostTexturePixels,
    /// Background alone, same size as `pixels`, when
    /// [`TextRasterRequest::split_background`] is set.
    pub background: Option<Vec<u8>>,
}

/// Text geometry from [`TextRasterizer::measure`]. Rectangles are `[left, top, right, bottom]`
/// in pixels from the layout box's top-left, which sits at ([`TextTexture::layout_x`],
/// [`TextTexture::layout_y`]) in the rasterized texture.
#[derive(Clone, Debug, PartialEq)]
pub struct TextMeasure {
    /// The wrap width, else the widest line, rounded up.
    pub box_width: u32,
    /// All lines' em boxes, rounded up.
    pub box_height: u32,
    /// [`TextRasterRequest::decoration_margin`], horizontally and vertically.
    pub decoration_margin: [u32; 2],
    /// Horizontal extent of the line advances by the box height, grown by the decoration
    /// margin. Unlike `ink`, it does not depend on glyph shapes.
    pub content: [f32; 4],
    /// Bounds of everything drawn (glyphs, strokes, background), at most a pixel loose (more for
    /// COLR clip boxes). `None` when nothing is drawn.
    pub ink: Option<[f32; 4]>,
    /// Number of laid-out lines.
    pub line_count: u32,
    /// First alphabetic baseline, in pixels from the layout box's top.
    pub first_baseline: f32,
    /// Last alphabetic baseline, in pixels from the layout box's top.
    pub last_baseline: f32,
    /// Whether [`TextRasterRequest::clamp`] discarded text.
    pub truncated: bool,
}

/// Text shaped once ([`TextRasterizer::shape`]) so lines can be broken at any width without
/// reshaping.
#[derive(Clone, Debug)]
pub struct ShapedText {
    paragraphs: Vec<Vec<Cluster>>,
    font_size: f32,
    line_height: f32,
}

/// Lines of a shaped text at one wrapping width ([`ShapedText::lines_at`]).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LineMetrics {
    /// Number of lines.
    pub line_count: u32,
    /// Widest line advance, excluding trailing spaces.
    pub widest: f32,
    /// Unrounded box height.
    pub height: f32,
    /// First alphabetic baseline, in pixels from the box's top.
    pub first_baseline: f32,
    /// Last alphabetic baseline, in pixels from the box's top.
    pub last_baseline: f32,
}

impl ShapedText {
    /// CSS min-content width: the widest unbreakable run.
    #[must_use]
    pub fn min_content_width(&self) -> f32 {
        let mut widest = 0.0_f32;
        for clusters in &self.paragraphs {
            let mut start = 0;
            for index in 1..=clusters.len() {
                if index == clusters.len() || clusters[index].break_before {
                    widest = widest.max(advance(&clusters[start..index]));
                    start = index;
                }
            }
        }
        widest
    }

    /// CSS max-content width: the widest unwrapped paragraph.
    #[must_use]
    pub fn max_content_width(&self) -> f32 {
        self.paragraphs
            .iter()
            .map(|clusters| advance(clusters))
            .fold(0.0, f32::max)
    }

    /// Lines at `max_width`; a non-positive width does not wrap.
    #[must_use]
    pub fn lines_at(&self, max_width: f32) -> LineMetrics {
        self.lines_within(max_width, None).lines
    }

    /// [`line_limit`] for this text's font size and line height, with its lines broken at
    /// `max_width` (non-positive does not wrap) when their sizes differ.
    #[must_use]
    pub fn line_limit(
        &self,
        max_width: f32,
        max_lines: Option<u32>,
        max_height: f32,
    ) -> Option<u32> {
        if self.is_uniform() {
            return line_limit(self.font_size, self.line_height, max_lines, max_height);
        }
        let sizes = self
            .paragraphs
            .iter()
            .flat_map(|clusters| {
                break_lines(clusters, max_width)
                    .into_iter()
                    .map(|range| line_size(&clusters[range], self.font_size))
            })
            .collect::<Vec<_>>();
        mixed_line_limit(&sizes, self.line_height, max_lines, max_height)
    }

    fn is_uniform(&self) -> bool {
        self.paragraphs
            .iter()
            .flatten()
            .all(|cluster| cluster.size <= self.font_size)
    }

    /// Lines at `max_width` (non-positive does not wrap), cut after `limit` lines as a
    /// [`TextClamp`] cuts them. The ellipsis takes no part in the widths.
    #[must_use]
    pub fn lines_within(&self, max_width: f32, limit: Option<u32>) -> ClampedLines {
        let limit = limit.map_or(u32::MAX, |limit| limit.max(1));
        let (mut count, mut kept, mut widest) = (0_u32, 1_u32, 0.0_f32);
        let mut sizes = Vec::new();
        let (mut truncated, mut emergency) = (false, false);
        'paragraphs: for (index, clusters) in self.paragraphs.iter().enumerate() {
            let ranges = break_lines(clusters, max_width);
            for (at, range) in ranges.iter().enumerate() {
                count = count.saturating_add(1);
                sizes.push(line_size(&clusters[range.clone()], self.font_size));
                if !range.is_empty() {
                    kept = count;
                }
                widest = widest.max(advance(&clusters[range.clone()]));
                emergency |= !range.is_empty()
                    && range.end < clusters.len()
                    && !clusters[range.end].break_before;
                if count == limit {
                    truncated = ranges[at + 1..].iter().any(|range| !range.is_empty())
                        || self.paragraphs[index + 1..]
                            .iter()
                            .any(|clusters| !clusters.is_empty());
                    break 'paragraphs;
                }
            }
        }
        sizes.truncate(kept as usize);
        let boxes = LineBoxes::of(&sizes, self.font_size, self.line_height);
        let last = sizes.len().saturating_sub(1);
        ClampedLines {
            lines: LineMetrics {
                line_count: kept,
                widest,
                height: boxes.height,
                first_baseline: boxes.tops[0] + boxes.ascents[0],
                last_baseline: boxes.tops[last] + boxes.ascents[last],
            },
            truncated,
            splits_word: if truncated {
                emergency
            } else {
                max_width > 0.0 && self.min_content_width() > max_width
            },
        }
    }
}

/// The lines a clamp keeps ([`ShapedText::lines_within`]).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ClampedLines {
    /// The kept lines.
    pub lines: LineMetrics,
    /// Whether lines with content were cut off.
    pub truncated: bool,
    /// Whether a kept line breaks inside a word. Uncut text breaks one when its min-content
    /// width exceeds the wrapping width.
    pub splits_word: bool,
}

/// Line advance as [`break_lines`] sums it, excluding hanging clusters and the last cluster's
/// letter spacing.
fn advance(clusters: &[Cluster]) -> f32 {
    let Some(last) = clusters.iter().rposition(|cluster| !cluster.hangs) else {
        return 0.0;
    };
    clusters[..=last]
        .iter()
        .fold(0.0, |width, cluster| width + cluster.width)
        - clusters[last].spacing
}

/// CSS half-leading above the em box, or zero when lines are closer than an em.
#[must_use]
pub fn half_leading(font_size: f32, line_height: f32) -> f32 {
    (font_size * line_height - font_size).max(0.0) * 0.5
}

/// Box height covering every line's em box, including the last when lines overlap.
#[allow(clippy::cast_precision_loss)]
fn box_height(line_count: u32, line_advance: f32, font_size: f32) -> f32 {
    line_count.saturating_sub(1) as f32 * line_advance + line_advance.max(font_size)
}

/// Typed text-host failure.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum TextRasterError {
    /// No usable fonts were registered.
    NoFonts,
    /// OpenType parsing failed.
    InvalidFont(String),
    /// Request values are non-finite or outside the supported range.
    InvalidRequest,
    /// Computed dimensions cannot be represented safely.
    TooLarge,
}

impl fmt::Display for TextRasterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoFonts => formatter.write_str("text rasterizer requires at least one font"),
            Self::InvalidFont(error) => write!(formatter, "invalid text font: {error}"),
            Self::InvalidRequest => formatter.write_str("invalid text raster request"),
            Self::TooLarge => formatter.write_str("text raster result is too large"),
        }
    }
}

impl std::error::Error for TextRasterError {}

#[derive(Clone, Debug)]
struct LoadedFont {
    source: TextFont,
    /// Folded [`font_family`], used to group faces.
    family: Option<String>,
    /// Folded names in every language, matched against requested families.
    aliases: Vec<String>,
    /// OS/2 usWeightClass in [1, 1000]; 400 without the table.
    weight: u16,
    /// The fvar `wght` axis (min, default, max), for a variable face.
    wght: Option<(f32, f32, f32)>,
    /// The fvar `ital` axis (min, default, max), for a variable face.
    ital: Option<(f32, f32, f32)>,
    /// OS/2 `fsSelection` italic bit, or a non-zero `post.italicAngle`.
    italic: bool,
    /// OS/2 `fsSelection` oblique bit.
    oblique: bool,
}

const WGHT: ttf_parser::Tag = ttf_parser::Tag::from_bytes(b"wght");
const ITAL: ttf_parser::Tag = ttf_parser::Tag::from_bytes(b"ital");

impl LoadedFont {
    /// Validates the face and reads its family and weight; outlines are read lazily.
    fn parse(source: TextFont) -> Result<Self, TextRasterError> {
        let face = parse_checked(&source.bytes, source.face_index)?;
        let weight = face.weight().to_number().clamp(1, 1000);
        let axis = |tag| {
            face.variation_axes()
                .into_iter()
                .find(|axis| axis.tag == tag)
                .map(|axis| (axis.min_value, axis.def_value, axis.max_value))
        };
        let (wght, ital) = (axis(WGHT), axis(ITAL));
        let family = font_family(&source.bytes, source.face_index).map(|name| fold_family(&name));
        let mut aliases = font_family_names(&source.bytes, source.face_index)
            .iter()
            .map(|name| fold_family(name))
            .collect::<Vec<_>>();
        aliases.extend(family.clone());
        aliases.sort();
        aliases.dedup();
        Ok(Self {
            italic: face.is_italic(),
            oblique: face.is_oblique(),
            source,
            family,
            aliases,
            weight,
            wght,
            ital,
        })
    }

    fn ital_reaches_one(&self) -> bool {
        self.ital.is_some_and(|(_, _, max)| max >= 1.0)
    }

    /// Rank of the face for `style`, CSS Fonts 4 section 5.2: lower narrows first.
    fn style_rank(&self, style: FontStyle) -> u8 {
        let italic = self.italic || self.ital_reaches_one();
        let (first, second) = match style {
            FontStyle::Normal => (!self.italic && !self.oblique, self.oblique && !self.italic),
            FontStyle::Italic => (italic, self.oblique),
            FontStyle::Oblique => (self.oblique, italic),
        };
        u8::from(!first) + u8::from(!first && !second)
    }

    /// Weight range: the `wght` axis rounded inward, or the static weight twice.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn weights(&self) -> (u16, u16) {
        self.wght
            .map_or((self.weight, self.weight), |(min, _, max)| {
                let whole = |value: f32| value.clamp(1.0, 1000.0) as u16;
                (whole(min.ceil()), whole(max.floor()).max(whole(min.ceil())))
            })
    }
}

/// Face indices in CSS Fonts 4 (section 5.2) weight matching order for `desired`, ties in input
/// order. A range containing `desired` matches exactly; otherwise its nearest end counts.
fn weight_order(weights: &[(u16, u16)], desired: u16) -> Vec<usize> {
    let key = |&(min, max): &(u16, u16)| -> (u8, u16) {
        if (min..=max).contains(&desired) {
            return (0, 0);
        }
        let weight = if desired < min { min } else { max };
        let (above, distance) = (weight > desired, weight.abs_diff(desired));
        if (400..=500).contains(&desired) {
            match (above, weight <= 500) {
                (true, true) => (1, distance),
                (false, _) => (2, distance),
                (true, false) => (3, distance),
            }
        } else if (desired < 400) != above {
            (1, distance)
        } else {
            (2, distance)
        }
    };
    let mut order = (0..weights.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| key(&weights[index]));
    order
}

/// Checks that `bytes` hold an OpenType face at `face_index`, as [`TextRasterizer::add_font`]
/// would, so a host can reject a file at import.
///
/// Fonts are untrusted input: a collection may hold 256 faces, a glyph may expand to 1,024
/// simple glyphs through its components, and a color glyph's painting may visit 16,384 paints.
///
/// # Errors
/// Returns [`TextRasterError::InvalidFont`] with the parser's or the budget's reason.
pub fn check_font(bytes: &[u8], face_index: u32) -> Result<(), TextRasterError> {
    parse_checked(bytes, face_index).map(drop)
}

fn parse_checked(bytes: &[u8], face_index: u32) -> Result<ttf_parser::Face<'_>, TextRasterError> {
    if ttf_parser::fonts_in_collection(bytes)
        .is_some_and(|faces| faces > font_check::MAX_COLLECTION_FACES)
    {
        return Err(TextRasterError::InvalidFont(format!(
            "the font collection holds more than {} faces",
            font_check::MAX_COLLECTION_FACES
        )));
    }
    let face = ttf_parser::Face::parse(bytes, face_index)
        .map_err(|error| TextRasterError::InvalidFont(error.to_string()))?;
    font_check::check_face(&face).map_err(TextRasterError::InvalidFont)?;
    Ok(face)
}

/// Number of faces in a font collection (`.ttc`, `.otc`), else 1.
#[must_use]
pub fn face_count(bytes: &[u8]) -> u32 {
    ttf_parser::fonts_in_collection(bytes).unwrap_or(1)
}

/// The face's typographic family (name ID 16), else family (name ID 1), preferring US English.
/// `None` for unparsable bytes or no family name.
#[must_use]
pub fn font_family(bytes: &[u8], face_index: u32) -> Option<String> {
    use rustybuzz::ttf_parser::{Language, name_id};
    let face = rustybuzz::ttf_parser::Face::parse(bytes, face_index).ok()?;
    let names = face.names();
    [name_id::TYPOGRAPHIC_FAMILY, name_id::FAMILY]
        .into_iter()
        .find_map(|id| {
            let decoded = || {
                names
                    .into_iter()
                    .filter(move |name| name.name_id == id)
                    .filter_map(|name| {
                        let text = name.to_string()?;
                        (!text.trim().is_empty()).then_some((name.language(), text))
                    })
            };
            decoded()
                .find(|(language, _)| *language == Language::English_UnitedStates)
                .or_else(|| decoded().next())
                .map(|(_, text)| text)
        })
}

/// Every distinct name ID 16 and 1 family name in every language, in table order. A project may
/// name a family by any of them. Empty for unparsable bytes.
#[must_use]
pub fn font_family_names(bytes: &[u8], face_index: u32) -> Vec<String> {
    use rustybuzz::ttf_parser::name_id;
    let Ok(face) = rustybuzz::ttf_parser::Face::parse(bytes, face_index) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for name in face.names() {
        if name.name_id != name_id::TYPOGRAPHIC_FAMILY && name.name_id != name_id::FAMILY {
            continue;
        }
        if let Some(text) = name.to_string().map(|text| text.trim().to_owned())
            && !text.is_empty()
            && !names.contains(&text)
        {
            names.push(text);
        }
    }
    names
}

/// Normalizes a family name for comparison: NFC, then Unicode lowercase.
#[must_use]
pub fn fold_family(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.trim().nfc().collect::<String>().to_lowercase()
}

/// Default result cache budget in bytes: a few full-frame 4K titles.
pub const DEFAULT_TEXT_CACHE_BYTES: usize = 64 << 20;

/// Per-entry cost so that empty results still bound the entry count.
const CACHE_ENTRY_OVERHEAD: usize = 256;

/// Deterministic text host with version-key result caching.
///
/// Results are LRU-cached by content version within a byte budget
/// ([`DEFAULT_TEXT_CACHE_BYTES`] by default) and shared as the same [`Arc`].
#[derive(Debug)]
pub struct TextRasterizer {
    fonts: Vec<LoadedFont>,
    cache: ResultCache,
    tiles: animated::TileCache,
    unmatched: std::sync::Mutex<BTreeSet<String>>,
}

#[derive(Debug)]
struct ResultCache {
    entries: HashMap<u64, CacheEntry>,
    /// Content versions by last use, oldest first.
    by_use: BTreeMap<u64, u64>,
    next_use: u64,
    bytes: usize,
    budget: usize,
}

#[derive(Debug)]
struct CacheEntry {
    result: Arc<TextRasterResult>,
    last_use: u64,
    bytes: usize,
}

impl ResultCache {
    fn new(budget: usize) -> Self {
        Self {
            entries: HashMap::new(),
            by_use: BTreeMap::new(),
            next_use: 0,
            bytes: 0,
            budget,
        }
    }

    fn get(&mut self, version: u64) -> Option<Arc<TextRasterResult>> {
        let entry = self.entries.get_mut(&version)?;
        self.by_use.remove(&entry.last_use);
        entry.last_use = self.next_use;
        self.by_use.insert(self.next_use, version);
        self.next_use += 1;
        Some(Arc::clone(&entry.result))
    }

    fn insert(&mut self, version: u64, result: &Arc<TextRasterResult>) {
        self.remove(version);
        let bytes = result
            .pixels
            .rgba8
            .len()
            .saturating_add(CACHE_ENTRY_OVERHEAD);
        if bytes > self.budget {
            return;
        }
        self.bytes += bytes;
        self.entries.insert(
            version,
            CacheEntry {
                result: Arc::clone(result),
                last_use: self.next_use,
                bytes,
            },
        );
        self.by_use.insert(self.next_use, version);
        self.next_use += 1;
        self.evict();
    }

    fn remove(&mut self, version: u64) {
        if let Some(entry) = self.entries.remove(&version) {
            self.by_use.remove(&entry.last_use);
            self.bytes -= entry.bytes;
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.by_use.clear();
        self.bytes = 0;
    }

    fn set_budget(&mut self, budget: usize) {
        self.budget = budget;
        self.evict();
    }

    fn evict(&mut self) {
        while self.bytes > self.budget {
            let Some((_, version)) = self.by_use.pop_first() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&version) {
                self.bytes -= entry.bytes;
            }
        }
    }
}

impl TextRasterizer {
    /// A rasterizer with no fonts yet; add them with [`Self::add_font`].
    #[must_use]
    pub fn empty() -> Self {
        Self {
            fonts: Vec::new(),
            cache: ResultCache::new(DEFAULT_TEXT_CACHE_BYTES),
            tiles: animated::TileCache::default(),
            unmatched: std::sync::Mutex::default(),
        }
    }

    /// Takes a fallback-ordered OpenType stack.
    ///
    /// # Errors
    /// Returns [`TextRasterError::NoFonts`] or the first malformed font.
    pub fn new(fonts: Vec<TextFont>) -> Result<Self, TextRasterError> {
        if fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let loaded = fonts
            .into_iter()
            .map(LoadedFont::parse)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            fonts: loaded,
            cache: ResultCache::new(DEFAULT_TEXT_CACHE_BYTES),
            tiles: animated::TileCache::default(),
            unmatched: std::sync::Mutex::default(),
        })
    }

    /// Appends a font to the end of the fallback stack.
    ///
    /// Clears the result cache.
    ///
    /// # Errors
    /// Returns [`TextRasterError::InvalidFont`] for malformed font bytes.
    pub fn add_font(&mut self, source: TextFont) -> Result<(), TextRasterError> {
        self.fonts.push(LoadedFont::parse(source)?);
        self.cache.clear();
        self.tiles.clear();
        Ok(())
    }

    /// Number of fonts in the fallback stack.
    #[must_use]
    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    /// Requested families no registered font had, sorted, as spelled. Adding fonts later does not
    /// clear the list.
    #[must_use]
    pub fn unmatched_families(&self) -> Vec<String> {
        self.unmatched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    /// Shapes, lays out and rasterizes one request, or returns the cached result for its version.
    ///
    /// # Errors
    /// Returns a typed validation, font, or size error.
    pub fn rasterize(
        &mut self,
        request: &TextRasterRequest,
    ) -> Result<Arc<TextRasterResult>, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        if let Some(cached) = self.cache.get(request.content_version) {
            return Ok(cached);
        }
        let result = if runs::paints_per_cluster(request) {
            let states = vec![ClusterState::IDENTITY; self.layout(request)?.clusters.len()];
            let mut tiles = std::mem::take(&mut self.tiles);
            let result = self.rasterize_states(request, &states, &mut tiles);
            self.tiles = tiles;
            result?
        } else {
            self.rasterize_uncached(request)?
        };
        let result = Arc::new(result);
        self.cache.insert(request.content_version, &result);
        Ok(result)
    }

    /// Drops a single host version after content or font invalidation.
    pub fn invalidate(&mut self, content_version: u64) {
        self.cache.remove(content_version);
    }

    /// Sets the result cache budget in bytes, evicting down to it. Results larger than the
    /// budget are returned but not kept.
    pub fn set_cache_budget(&mut self, bytes: usize) {
        self.cache.set_budget(bytes);
    }

    /// Bytes the cached results count against the budget.
    #[must_use]
    pub fn cache_bytes(&self) -> usize {
        self.cache.bytes
    }

    /// Drops every rasterized result while retaining parsed fonts.
    pub fn clear(&mut self) {
        self.cache.clear();
        self.tiles.clear();
    }

    /// Number of versioned raster results currently retained.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.cache.entries.len()
    }

    /// Measures `request` as [`Self::rasterize`] would lay it out, without drawing or caching.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::rasterize`].
    #[allow(clippy::cast_precision_loss)]
    pub fn measure(&self, request: &TextRasterRequest) -> Result<TextMeasure, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let (faces, shaping) = self.prepare(request)?;
        let layout = Self::lay_out(&faces, &shaping, request)?;
        let mut inks = vec![None; shaping.looks.len()];
        layout.each_glyph(
            &faces,
            &shaping,
            request,
            &mut HashMap::new(),
            |drawing, origin, scale, glyph| {
                if let Some([x_min, y_min, x_max, y_max]) = drawing.ink_bounds() {
                    include(
                        &mut inks[usize::from(glyph.look)],
                        [
                            origin[0] + x_min * scale,
                            origin[1] - y_max * scale,
                            origin[0] + x_max * scale,
                            origin[1] - y_min * scale,
                        ],
                    );
                }
            },
        );
        let paint = layout.paint(&faces, &shaping, request.align);
        for decoration in &paint.decorations {
            let look = layout.clusters[decoration.owner].look;
            include(&mut inks[usize::from(look)], decoration.rect);
        }
        let mut ink = None;
        for (look, bounds) in shaping.looks.iter().zip(inks) {
            // The outermost band reaches the disc radius plus one rim pixel past the ink.
            let band = stroke_radius(&look.strokes).map_or(0.0, |radius| radius + 1.0);
            if let Some(bounds) = bounds {
                include(&mut ink, grown(bounds, band));
            }
        }
        for run in &paint.boxes {
            include(&mut ink, [run.left, run.top, run.right, run.bottom]);
        }
        let mut content = None;
        for (index, line) in layout.lines.iter().enumerate() {
            let left = layout.line_offset(line, request.align);
            let top = layout.line_top(index);
            include(&mut content, [left, 0.0, left + line.width, layout.height]);
            if let (Some(background), true) = (
                request
                    .background
                    .as_ref()
                    .filter(|background| background.extent != BoxExtent::Block),
                line.width > 0.0,
            ) {
                let [pad_top, pad_right, pad_bottom, pad_left] = background.padding;
                include(
                    &mut ink,
                    [
                        left - pad_left,
                        top - pad_top,
                        left + line.width + pad_right,
                        top + layout.line_size(index) + pad_bottom,
                    ],
                );
            }
        }
        if let Some((background, (left, width, top, height))) = request
            .background
            .as_ref()
            .filter(|background| background.extent == BoxExtent::Block)
            .zip(layout.block_box())
        {
            let [pad_top, pad_right, pad_bottom, pad_left] = background.padding;
            include(
                &mut ink,
                [
                    left - pad_left,
                    top - pad_top,
                    left + width + pad_right,
                    top + height + pad_bottom,
                ],
            );
        }
        let content = content.unwrap_or([0.0, 0.0, 0.0, layout.height]);
        let last = layout.lines.len().saturating_sub(1);
        Ok(TextMeasure {
            first_baseline: layout.baseline(0),
            last_baseline: layout.baseline(last),
            box_width: layout.box_width,
            box_height: layout.box_height,
            decoration_margin: request.decoration_margin(),
            content: grown_by(content, request.decoration_margin().map(|side| side as f32)),
            ink,
            line_count: layout.line_count,
            truncated: layout.truncated,
        })
    }

    /// Shapes `request` as [`Self::rasterize`] would, so lines can be broken at any width later.
    /// `max_width`, fill, strokes, background and content version are ignored.
    ///
    /// # Errors
    /// Returns [`TextRasterError::NoFonts`] without fonts, and
    /// [`TextRasterError::InvalidRequest`] for values [`Self::rasterize`] rejects.
    pub fn shape(&self, request: &TextRasterRequest) -> Result<ShapedText, TextRasterError> {
        validate_request(&TextRasterRequest {
            text: String::new(),
            max_width: 0.0,
            content_version: 1,
            fill: TextFill::Solid([0; 4]),
            strokes: Vec::new(),
            background: None,
            spans: Vec::new(),
            ..request.clone()
        })?;
        if request.text.len() > MAX_TEXT_BYTES || !runs::valid_spans(request) {
            return Err(TextRasterError::InvalidRequest);
        }
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let (faces, shaping) = self.prepare(request)?;
        let mut start = 0;
        let paragraphs = normalize(&request.text, request.emoji_policy)
            .split('\n')
            .map(|paragraph| {
                let shaped = ShapedParagraph::new(
                    &faces,
                    paragraph,
                    None,
                    &shaping,
                    Some(start),
                    request.emoji_policy,
                );
                start += paragraph.len() + 1;
                shaped.clusters
            })
            .collect();
        Ok(ShapedText {
            paragraphs,
            font_size: request.font_size,
            line_height: request.line_height,
        })
    }

    fn prepare(
        &self,
        request: &TextRasterRequest,
    ) -> Result<(Faces<'_>, runs::Shaping), TextRasterError> {
        let mut faces = Faces::new(&self.fonts, request.font_weight)?;
        let mut map = Vec::new();
        if !request.spans.is_empty() {
            normalize_mapped(&request.text, request.emoji_policy, Some(&mut map));
        }
        let (looks, at) = runs::Shaping::looks(request, &map);
        let mut orders = Vec::<Vec<usize>>::with_capacity(looks.len());
        for (index, look) in looks.iter().enumerate() {
            if let Some(same) = looks[..index].iter().position(|held| {
                held.family == look.family && held.weight == look.weight && held.style == look.style
            }) {
                orders.push(orders[same].clone());
                continue;
            }
            let fonts = self.font_order(look.family.as_deref(), look.style, look.weight);
            let italic = look.style != FontStyle::Normal;
            orders.push(
                fonts
                    .into_iter()
                    .map(|font| faces.instance(font, look.weight, italic))
                    .collect::<Result<_, _>>()?,
            );
        }
        Ok((faces, runs::Shaping::new(looks, orders, at)))
    }

    #[allow(clippy::cast_precision_loss, clippy::too_many_lines)]
    fn lay_out(
        faces: &Faces<'_>,
        shaping: &runs::Shaping,
        request: &TextRasterRequest,
    ) -> Result<Layout, TextRasterError> {
        let strut = request.font_size;
        let clamp = request.clamp.as_ref();
        let normalized = normalize(&request.text, request.emoji_policy);
        let paragraphs = normalized.split('\n').collect::<Vec<_>>();
        let starts = paragraphs
            .iter()
            .scan(0, |start, paragraph| {
                let at = *start;
                *start += paragraph.len() + 1;
                Some(at)
            })
            .collect::<Vec<_>>();
        let shape = |index: usize| {
            ShapedParagraph::new(
                faces,
                paragraphs[index],
                None,
                shaping,
                Some(starts[index]),
                request.emoji_policy,
            )
        };
        let mut shaped_ahead = Vec::new();
        let limit = clamp.and_then(|clamp| {
            if shaping.uniform() {
                return line_limit(
                    strut,
                    request.line_height,
                    clamp.max_lines,
                    clamp.max_height,
                );
            }
            shaped_ahead = (0..paragraphs.len())
                .map(|index| Some(shape(index)))
                .collect();
            let sizes = shaped_ahead
                .iter()
                .flatten()
                .flat_map(|shaped: &ShapedParagraph<'_>| {
                    break_lines(&shaped.clusters, request.wrap_width())
                        .into_iter()
                        .map(|range| line_size(&shaped.clusters[range], strut))
                })
                .collect::<Vec<_>>();
            mixed_line_limit(
                &sizes,
                request.line_height,
                clamp.max_lines,
                clamp.max_height,
            )
        });
        let limit = limit.map_or(usize::MAX, |limit| limit as usize);
        let mut lines = Vec::new();
        // Trailing blank lines take no room.
        let mut kept = 1;
        let mut clusters = Vec::new();
        let mut truncated = false;
        for index in 0..paragraphs.len() {
            let first = clusters.len();
            let shaped = shaped_ahead
                .get_mut(index)
                .and_then(Option::take)
                .unwrap_or_else(|| shape(index));
            let ranges = break_lines(&shaped.clusters, request.wrap_width());
            let mut clamped = false;
            for (at, range) in ranges.iter().enumerate() {
                let at_line = (first, lines.len());
                clamped = lines.len() + 1 >= limit;
                let cut = clamped
                    && (ranges[at + 1..].iter().any(|range| !range.is_empty())
                        || paragraphs[index + 1..]
                            .iter()
                            .any(|paragraph| !paragraph.is_empty()));
                truncated |= cut;
                let ellipsis = clamp
                    .and_then(|clamp| clamp.ellipsis.as_deref())
                    .filter(|_| cut);
                let size = line_size(&shaped.clusters[range.clone()], strut);
                let mut line = match ellipsis {
                    Some(ellipsis) => {
                        let level = shaped.bidi.paragraph_level;
                        let mark = ShapedParagraph::new(
                            faces,
                            ellipsis,
                            Some(level),
                            shaping,
                            None,
                            request.emoji_policy,
                        );
                        shaped.place_cut(
                            range.clone(),
                            &mark,
                            request.wrap_width(),
                            at_line,
                            &mut clusters,
                        )
                    }
                    None => shaped.place(range.clone(), at_line, &mut clusters),
                };
                line.size = size;
                if line.has_glyphs {
                    kept = lines.len() + 1;
                }
                lines.push(line);
                if clamped {
                    break;
                }
            }
            for span in &mut clusters[first..] {
                span.bytes = span.bytes.start + starts[index]..span.bytes.end + starts[index];
            }
            if clamped {
                break;
            }
        }
        lines.truncate(kept);
        let line_count = u32::try_from(lines.len()).map_err(|_| TextRasterError::TooLarge)?;
        let width = if request.max_width > 0.0 {
            request.max_width
        } else {
            lines.iter().map(|line| line.width).fold(0.0, f32::max)
        }
        .max(1.0);
        let sizes = lines.iter().map(|line| line.size).collect::<Vec<_>>();
        let metrics = LineBoxes::of(&sizes, strut, request.line_height);
        let height = metrics.height.max(1.0);
        Ok(Layout {
            box_width: checked_dimension(width)?,
            box_height: checked_dimension(height)?,
            lines,
            clusters,
            line_count,
            boxes: metrics,
            ascent: strut * 0.8,
            descent: strut * 0.2,
            width,
            height,
            truncated,
        })
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        clippy::too_many_lines
    )]
    fn rasterize_uncached(
        &self,
        request: &TextRasterRequest,
    ) -> Result<TextRasterResult, TextRasterError> {
        let (faces, shaping) = self.prepare(request)?;
        let layout = Self::lay_out(&faces, &shaping, request)?;
        let (box_width, box_height) = (layout.box_width, layout.box_height);
        let mut drawings = HashMap::<DrawingKey, Drawing>::new();
        let mut placed = Vec::new();
        let (mut overflow_x, mut overflow_y) = (0_i64, 0_i64);
        let (mut ink_ascent, mut ink_descent) = (layout.ascent, layout.descent);
        layout.each_glyph(
            &faces,
            &shaping,
            request,
            &mut drawings,
            |drawing, origin, scale, glyph| {
                let Some(bounds) = drawing.bounds() else {
                    return;
                };
                let pixels = PixelBox::of(bounds, origin, scale);
                overflow_x = overflow_x
                    .max(pixels.left.saturating_neg())
                    .max(pixels.right.saturating_sub(i64::from(box_width)));
                overflow_y = overflow_y
                    .max(pixels.top.saturating_neg())
                    .max(pixels.bottom.saturating_sub(i64::from(box_height)));
                ink_ascent = ink_ascent.max(bounds[3] * scale + glyph.y_offset);
                ink_descent = ink_descent.max(-(bounds[1] * scale + glyph.y_offset));
                placed.push((drawing_key(&shaping, glyph), origin, scale, pixels));
            },
        );
        // A symmetric margin keeps the box centered and leaves room for decorations and overflow.
        let (outline, padding) = decoration_extents(request);
        let margin = |overflow: i64, padding: u32| -> Result<u32, TextRasterError> {
            u32::try_from(overflow)
                .map_err(|_| TextRasterError::TooLarge)?
                .checked_add(outline)
                .map(|margin| margin.max(outline.max(padding)))
                .ok_or(TextRasterError::TooLarge)
        };
        let (margin_x, margin_y) = (
            margin(overflow_x, padding[0])?,
            margin(overflow_y, padding[1])?,
        );
        let width = margin_x
            .checked_mul(2)
            .and_then(|margins| box_width.checked_add(margins))
            .ok_or(TextRasterError::TooLarge)?;
        let height = margin_y
            .checked_mul(2)
            .and_then(|margins| box_height.checked_add(margins))
            .ok_or(TextRasterError::TooLarge)?;
        let colors = placed
            .iter()
            .any(|(key, ..)| !matches!(drawings[key], Drawing::Outline(_)));
        let pixel_count = raster_pixels(
            width,
            height,
            raster_bytes_per_pixel(request, request.strokes.len(), colors),
        )?;
        let mut coverage = vec![0_u8; pixel_count];
        // Allocated only once a color glyph is placed.
        let mut colors: Option<Vec<u8>> = None;
        let line_boxes = layout.background_boxes(request.align, request.background.as_ref());
        let highlight = request
            .highlight
            .as_ref()
            .map_or_else(Vec::new, |highlight| {
                let clusters = animated::text_layout(&layout, &shaping, request).clusters;
                highlight::boxes(
                    &layout,
                    &clusters,
                    &layout.static_extents(request.align),
                    highlight,
                )
            });
        for (key, origin, scale, pixels) in placed {
            let (origin_x, origin_y) = (
                pixels.left + i64::from(margin_x),
                pixels.top + i64::from(margin_y),
            );
            let glyph_width = usize::try_from(pixels.right - pixels.left).unwrap_or(0);
            match &drawings[&key] {
                Drawing::Outline(outline) => place_glyph(
                    &mut coverage,
                    width,
                    height,
                    origin_x,
                    origin_y,
                    glyph_width,
                    &outline.draw(origin, scale, pixels),
                ),
                drawing => place_color_glyph(
                    colors.get_or_insert_with(|| vec![0; pixel_count * 4]),
                    width,
                    height,
                    origin_x,
                    origin_y,
                    glyph_width,
                    &drawing.draw_color(origin, scale, pixels),
                ),
            }
        }
        let bands = stroke_bands(request, &coverage, colors.as_deref(), width);
        let (rgba8, background) = compose(
            request,
            &coverage,
            colors.as_deref(),
            width,
            [margin_x, margin_y],
            &line_boxes,
            &[],
            &highlight,
            &bands,
            &RequestLook::of(request, [layout.width, layout.height]),
        );
        let texture = HostTexture {
            content_version: request.content_version,
            width,
            height,
            premultiplication: Premultiplication::Premultiplied,
            color_space: RasterColorSpace::Srgb,
        };
        Ok(TextRasterResult {
            descriptor: TextTexture {
                texture: texture.clone(),
                layout_x: margin_x as f32,
                layout_y: margin_y as f32,
                layout_width: layout.width,
                layout_height: layout.height,
                first_baseline: layout.baseline(0),
                ascent: ink_ascent,
                descent: ink_descent,
                line_count: layout.line_count,
            },
            pixels: HostTexturePixels { texture, rgba8 },
            background,
        })
    }

    /// Font try order: `family` first, then families in registration order; within a family,
    /// faces by style ([`LoadedFont::style_rank`]), then by [`weight_order`].
    fn font_order(&self, family: Option<&str>, style: FontStyle, weight: u16) -> Vec<usize> {
        // A face without a family is its own group.
        let mut groups: Vec<(Option<&str>, Vec<usize>)> = Vec::new();
        for (index, font) in self.fonts.iter().enumerate() {
            let name = font.family.as_deref();
            match groups
                .iter_mut()
                .find(|(held, _)| name.is_some() && *held == name)
            {
                Some((_, faces)) => faces.push(index),
                None => groups.push((name, vec![index])),
            }
        }
        if let Some(requested) = family {
            let wanted = fold_family(requested);
            match groups.iter().position(|(_, faces)| {
                faces
                    .iter()
                    .any(|&index| self.fonts[index].aliases.contains(&wanted))
            }) {
                Some(at) => {
                    let preferred = groups.remove(at);
                    groups.insert(0, preferred);
                }
                None if !self.fonts.is_empty() => {
                    self.unmatched
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .insert(requested.to_owned());
                }
                None => {}
            }
        }
        groups
            .into_iter()
            .flat_map(|(_, faces)| {
                let weights = faces
                    .iter()
                    .map(|&index| self.fonts[index].weights())
                    .collect::<Vec<_>>();
                let mut order = weight_order(&weights, weight);
                order.sort_by_key(|&at| self.fonts[faces[at]].style_rank(style));
                order.into_iter().map(move |at| faces[at])
            })
            .collect()
    }
}

/// Faces parsed once per request; shaping tables load lazily.
struct Faces<'a> {
    fonts: &'a [LoadedFont],
    parsed: Vec<ttf_parser::Face<'a>>,
    shapers: Vec<OnceCell<rustybuzz::Face<'a>>>,
    /// Has a COLR, CBDT or sbix table.
    color: Vec<bool>,
    of: Vec<(usize, u16, bool)>,
}

impl<'a> Faces<'a> {
    /// Variable faces are set to `weight`, clamped to their axis.
    fn new(fonts: &'a [LoadedFont], weight: u16) -> Result<Self, TextRasterError> {
        let mut faces = Self {
            fonts,
            parsed: Vec::with_capacity(fonts.len()),
            shapers: Vec::with_capacity(fonts.len()),
            color: Vec::with_capacity(fonts.len()),
            of: Vec::with_capacity(fonts.len()),
        };
        for font in 0..fonts.len() {
            faces.push(font, weight, false)?;
        }
        Ok(faces)
    }

    fn push(&mut self, font: usize, weight: u16, italic: bool) -> Result<usize, TextRasterError> {
        let loaded = &self.fonts[font];
        let mut face = ttf_parser::Face::parse(&loaded.source.bytes, loaded.source.face_index)
            .ok()
            .filter(|face| face.units_per_em() > 0)
            .ok_or_else(|| TextRasterError::InvalidFont(loaded.source.family.clone()))?;
        if let Some((min, _, max)) = loaded.wght {
            face.set_variation(WGHT, f32::from(weight).clamp(min, max));
        }
        if italic && let Some((min, _, max)) = loaded.ital {
            face.set_variation(ITAL, 1.0_f32.clamp(min, max));
        }
        let tables = face.tables();
        self.color
            .push(tables.colr.is_some() || tables.cbdt.is_some() || tables.sbix.is_some());
        self.of
            .push((font, if loaded.wght.is_some() { weight } else { 0 }, italic));
        self.parsed.push(face);
        self.shapers.push(OnceCell::new());
        Ok(self.parsed.len() - 1)
    }

    fn instance(
        &mut self,
        font: usize,
        weight: u16,
        italic: bool,
    ) -> Result<usize, TextRasterError> {
        let loaded = &self.fonts[font];
        let italic = italic && loaded.ital_reaches_one();
        let weight = if loaded.wght.is_some() { weight } else { 0 };
        let (_, base_weight, _) = self.of[font];
        if weight == base_weight && !italic {
            return Ok(font);
        }
        if let Some(found) = self
            .of
            .iter()
            .position(|held| *held == (font, weight, italic))
        {
            return Ok(found);
        }
        self.push(font, weight.max(1), italic)
    }

    fn slanted(&self, face: usize) -> bool {
        let (font, _, italic) = self.of[face];
        italic || self.fonts[font].italic || self.fonts[font].oblique
    }

    fn shaper(&self, font: usize) -> &rustybuzz::Face<'a> {
        self.shapers[font].get_or_init(|| rustybuzz::Face::from_face(self.parsed[font].clone()))
    }

    #[allow(clippy::cast_precision_loss)]
    fn units_per_em(&self, font: usize) -> f32 {
        f32::from(self.parsed[font].units_per_em())
    }

    fn has(&self, font: usize, character: char) -> bool {
        self.parsed[font]
            .glyph_index(character)
            .is_some_and(|glyph| glyph.0 != 0)
    }

    /// Whether the font has every visible character of `grapheme`.
    fn covers(&self, font: usize, grapheme: &str) -> bool {
        grapheme
            .chars()
            .filter(|&character| !is_default_ignorable(character))
            .all(|character| self.has(font, character))
    }
}

/// Replaces emoji with a replacement character under [`EmojiPolicy::Replace`], every line or
/// paragraph separator (U+2028 included) with `\n`, and tabs with spaces.
fn normalize(text: &str, policy: EmojiPolicy) -> String {
    normalize_mapped(text, policy, None)
}

/// [`normalize`], optionally filling `map` with the source byte of each output byte (grapheme
/// start for replaced ones) plus a final entry for `text`'s length.
fn normalize_mapped(text: &str, policy: EmojiPolicy, mut map: Option<&mut Vec<usize>>) -> String {
    let mut normalized = String::with_capacity(text.len());
    for (start, grapheme) in text.grapheme_indices(true) {
        let before = normalized.len();
        if policy == EmojiPolicy::Replace && is_emoji(grapheme) {
            normalized.push('\u{fffd}');
        } else if grapheme == "\r\n" {
            normalized.push('\n');
        } else {
            normalized.extend(grapheme.chars().map(|character| match character {
                '\r'
                | '\u{b}'
                | '\u{c}'
                | '\u{1c}'..='\u{1e}'
                | '\u{85}'
                | '\u{2028}'
                | '\u{2029}' => '\n',
                '\t' => ' ',
                other => other,
            }));
        }
        if let Some(map) = map.as_deref_mut() {
            // A grapheme that changed length maps to its start.
            let written = normalized.len() - before;
            if written == grapheme.len() {
                map.extend(start..start + written);
            } else {
                map.extend(std::iter::repeat_n(start, written));
            }
        }
    }
    if let Some(map) = map {
        map.push(text.len());
    }
    normalized
}

/// Emoji presentation per UTS #51, matching `OffscreenTextRasterizer`: an `Emoji_Presentation`
/// character, or U+FE0F with a pictograph or keycap, unless U+FE0E asks for text.
fn is_emoji(grapheme: &str) -> bool {
    if grapheme.contains('\u{fe0e}') {
        return false;
    }
    let selected = grapheme.contains('\u{fe0f}');
    grapheme.chars().any(|character| {
        in_table(unicode_tables::EMOJI_PRESENTATION, character)
            || (selected
                && (character == '\u{20e3}'
                    || in_table(unicode_tables::EXTENDED_PICTOGRAPHIC, character)))
    })
}

/// Whether `text`'s first non-common script is cursive per CSS Text 3 (no letter spacing).
fn is_cursive(text: &str) -> bool {
    text.chars()
        .map(|character| character.script())
        .find(|script| !matches!(script, Script::Common | Script::Inherited | Script::Unknown))
        .is_some_and(|script| {
            matches!(
                script,
                Script::Adlam
                    | Script::Arabic
                    | Script::Hanifi_Rohingya
                    | Script::Mandaic
                    | Script::Mongolian
                    | Script::Nko
                    | Script::Phags_Pa
                    | Script::Syriac
            )
        })
}

fn is_default_ignorable(character: char) -> bool {
    in_table(unicode_tables::DEFAULT_IGNORABLE, character)
}

fn in_table(table: &[(u32, u32)], character: char) -> bool {
    let code = u32::from(character);
    table
        .binary_search_by(|&(first, last)| {
            if last < code {
                Ordering::Less
            } else if first > code {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// A space that hangs at a line end. Excludes no-break spaces and the visible Ogham space mark.
fn hangs(character: char) -> bool {
    character.is_whitespace()
        && !matches!(character, '\u{a0}' | '\u{2007}' | '\u{202f}' | '\u{1680}')
}

fn is_hangul(character: char) -> bool {
    matches!(
        character,
        '\u{1100}'..='\u{11ff}'
            | '\u{3130}'..='\u{318f}'
            | '\u{a960}'..='\u{a97f}'
            | '\u{ac00}'..='\u{d7a3}'
            | '\u{d7b0}'..='\u{d7ff}'
    )
}

/// Where lines of sizes `sizes` (each at least the strut, the request's size) lie, CSS 2.1
/// 10.8.1 with nominal metrics: a line is the inline box of its largest size `S`, `L * S` tall
/// for line height `L`, its em box (0.8 `S` above the baseline, 0.2 below) centred in it.
#[derive(Clone, Debug, PartialEq)]
struct LineBoxes {
    tops: Vec<f32>,
    ascents: Vec<f32>,
    sizes: Vec<f32>,
    height: f32,
}

impl LineBoxes {
    #[allow(clippy::cast_precision_loss)]
    fn of(sizes: &[f32], strut: f32, line_height: f32) -> Self {
        let count = u32::try_from(sizes.len()).unwrap_or(u32::MAX);
        if sizes.iter().all(|size| size.to_bits() == strut.to_bits()) {
            let advance = strut * line_height;
            let leading = half_leading(strut, line_height);
            return Self {
                tops: (0..sizes.len())
                    .map(|index| index as f32 * advance + leading)
                    .collect(),
                ascents: vec![strut * 0.8; sizes.len()],
                sizes: sizes.to_vec(),
                height: box_height(count, advance, strut),
            };
        }
        let mut tops = Vec::with_capacity(sizes.len());
        let mut top = 0.0_f32;
        for &size in sizes {
            tops.push(top + half_leading(size, line_height));
            top += size * line_height;
        }
        Self {
            tops,
            ascents: sizes.iter().map(|size| size * 0.8).collect(),
            sizes: sizes.to_vec(),
            height: mixed_height(sizes, line_height),
        }
    }
}

fn mixed_height(sizes: &[f32], line_height: f32) -> f32 {
    let Some((last, before)) = sizes.split_last() else {
        return 0.0;
    };
    before
        .iter()
        .fold(0.0_f32, |height, size| height + size * line_height)
        + (last * line_height).max(*last)
}

fn line_size(clusters: &[Cluster], strut: f32) -> f32 {
    clusters
        .iter()
        .fold(strut, |largest, cluster| largest.max(cluster.size))
}

fn mixed_line_limit(
    sizes: &[f32],
    line_height: f32,
    max_lines: Option<u32>,
    max_height: f32,
) -> Option<u32> {
    let by_height = (max_height > 0.0).then(|| {
        let fitting = (1..=sizes.len())
            .take_while(|&count| mixed_height(&sizes[..count], line_height) <= max_height)
            .last()
            .unwrap_or(1);
        u32::try_from(fitting).unwrap_or(u32::MAX)
    });
    match (max_lines.map(|lines| lines.max(1)), by_height) {
        (Some(count), Some(height)) => Some(count.min(height)),
        (count, height) => count.or(height),
    }
}

/// Broken and placed lines, in pixels from the layout box's top-left, y down.
struct Layout {
    lines: Vec<LaidLine>,
    /// Logical order, bytes in the normalized text.
    clusters: Vec<ClusterSpan>,
    line_count: u32,
    boxes: LineBoxes,
    ascent: f32,
    descent: f32,
    /// The wrap width, else the widest line.
    width: f32,
    height: f32,
    box_width: u32,
    box_height: u32,
    /// Whether the request's clamp discarded lines.
    truncated: bool,
}

impl Layout {
    fn line_boxes(&self, align: TextAlign) -> Vec<(f32, f32, f32, f32)> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.width > 0.0)
            .map(|(index, line)| {
                (
                    self.line_offset(line, align),
                    line.width,
                    self.line_top(index),
                    self.line_size(index),
                )
            })
            .collect()
    }

    /// Every line's box, or the one block box, for `background`'s extent, where the static
    /// layout places the lines.
    fn background_boxes(
        &self,
        align: TextAlign,
        background: Option<&TextBackground>,
    ) -> Vec<(f32, f32, f32, f32)> {
        match background.map(|background| background.extent) {
            Some(BoxExtent::Block) => self.block_box().into_iter().collect(),
            _ => self.line_boxes(align),
        }
    }

    /// The box over the whole layout box: the wrap width by every line's box.
    fn block_box(&self) -> Option<(f32, f32, f32, f32)> {
        let last = self.lines.len().checked_sub(1)?;
        let top = self.line_top(0);
        Some((
            0.0,
            self.width,
            top,
            self.line_top(last) + self.line_size(last) - top,
        ))
    }

    /// Each cluster's advance where the static layout places it.
    fn static_extents(&self, align: TextAlign) -> Vec<Option<(f32, f32)>> {
        let mut extents = vec![None; self.clusters.len()];
        for index in 0..self.lines.len() {
            for (cluster, extent) in self.cluster_extents(index, align) {
                extents[cluster] = Some(extent);
            }
        }
        extents
    }

    fn cluster_extents(&self, index: usize, align: TextAlign) -> HashMap<usize, (f32, f32)> {
        let line = &self.lines[index];
        let offset = self.line_offset(line, align);
        let mut extents = HashMap::<usize, (f32, f32)>::new();
        for glyph in &line.glyphs {
            let left = glyph.x + offset;
            let extent = extents.entry(glyph.cluster).or_insert((left, left));
            *extent = (extent.0.min(left), extent.1.max(left + glyph.advance));
        }
        extents
    }

    fn paint(
        &self,
        faces: &Faces<'_>,
        shaping: &runs::Shaping,
        align: TextAlign,
    ) -> paint::LinePaint {
        let mut paint = paint::LinePaint::default();
        for index in 0..self.lines.len() {
            let extents = self.cluster_extents(index, align);
            paint.add_line(
                faces,
                shaping,
                self,
                index,
                (
                    |cluster| extents[&cluster],
                    |cluster| extents.get(&cluster).copied(),
                ),
                |_| animated::IDENTITY_AFFINE,
            );
        }
        paint
    }

    /// Start and End follow the paragraph's direction.
    fn line_offset(&self, line: &LaidLine, align: TextAlign) -> f32 {
        match (align, line.rtl) {
            (TextAlign::Center, _) => (self.width - line.width) * 0.5,
            (TextAlign::Start, false) | (TextAlign::End, true) => 0.0,
            (TextAlign::End, false) | (TextAlign::Start, true) => self.width - line.width,
        }
    }

    fn line_top(&self, index: usize) -> f32 {
        self.boxes.tops[index]
    }

    fn baseline(&self, index: usize) -> f32 {
        self.boxes.tops[index] + self.boxes.ascents[index]
    }

    fn line_size(&self, index: usize) -> f32 {
        self.boxes.sizes[index]
    }

    /// Calls `place` with each glyph's drawing (cached in `drawings`), origin and pixels per
    /// font unit.
    #[allow(clippy::cast_precision_loss)]
    fn each_glyph(
        &self,
        faces: &Faces<'_>,
        shaping: &runs::Shaping,
        request: &TextRasterRequest,
        drawings: &mut HashMap<DrawingKey, Drawing>,
        mut place: impl FnMut(&Drawing, [f32; 2], f32, &PositionedGlyph),
    ) {
        for (index, line) in self.lines.iter().enumerate() {
            let offset = self.line_offset(line, request.align);
            let baseline = self.baseline(index);
            for glyph in &line.glyphs {
                let key = drawing_key(shaping, glyph);
                let drawing = drawings
                    .entry(key)
                    .or_insert_with(|| Drawing::of(faces, key, request.emoji_policy));
                let scale = glyph.size / faces.units_per_em(glyph.font);
                let origin = [glyph.x + glyph.x_offset + offset, baseline - glyph.y_offset];
                place(drawing, origin, scale, glyph);
            }
        }
    }
}

fn include(bounds: &mut Option<[f32; 4]>, [left, top, right, bottom]: [f32; 4]) {
    let held = bounds.get_or_insert([left, top, right, bottom]);
    *held = [
        held[0].min(left),
        held[1].min(top),
        held[2].max(right),
        held[3].max(bottom),
    ];
}

fn grown([left, top, right, bottom]: [f32; 4], by: f32) -> [f32; 4] {
    [left - by, top - by, right + by, bottom + by]
}

fn grown_by([left, top, right, bottom]: [f32; 4], [x, y]: [f32; 2]) -> [f32; 4] {
    [left - x, top - y, right + x, bottom + y]
}

/// Byte range (paragraph-relative during layout, then text-relative) and line of a cluster.
#[derive(Clone, Debug)]
struct ClusterSpan {
    bytes: Range<usize>,
    line: usize,
    look: u16,
    /// The ellipsis of a cut line, with an empty range where the text was cut.
    ellipsis: bool,
}

struct LaidLine {
    /// Glyphs left to right, pens from the line's start.
    glyphs: Vec<PositionedGlyph>,
    /// Excludes hanging trailing spaces.
    width: f32,
    rtl: bool,
    /// Has glyphs, even if none with ink.
    has_glyphs: bool,
    size: f32,
}

/// A paragraph shaped and grouped into clusters, its lines not yet placed.
struct ShapedParagraph<'a> {
    text: &'a str,
    bidi: ParagraphBidiInfo<'a>,
    glyphs: Vec<ShapedGlyph>,
    clusters: Vec<Cluster>,
}

impl<'a> ShapedParagraph<'a> {
    /// Shapes `text` at `level`, else at the level its first strong character gives.
    fn new(
        faces: &Faces<'_>,
        text: &'a str,
        level: Option<unicode_bidi::Level>,
        shaping: &runs::Shaping,
        start: Option<usize>,
        policy: EmojiPolicy,
    ) -> Self {
        let look_of = |byte: usize| start.map_or(0, |start| shaping.at(start + byte));
        let bidi = ParagraphBidiInfo::new(text, level);
        let segments = segments(text, shaping, look_of);
        let glyphs = shape_paragraph(faces, text, &bidi, shaping, &segments, policy);
        let clusters = clusters(&glyphs, text, shaping, look_of);
        Self {
            text,
            bidi,
            glyphs,
            clusters,
        }
    }

    /// Places the clusters of `range` as line `line` in visual order. Their spans go to `spans`,
    /// where this paragraph's clusters are numbered from `cluster_base`.
    fn place(
        &self,
        range: Range<usize>,
        (cluster_base, line): (usize, usize),
        spans: &mut Vec<ClusterSpan>,
    ) -> LaidLine {
        self.push_spans(range.clone(), cluster_base, line, spans);
        self.position(range, cluster_base)
    }

    /// [`Self::place`] for the last line a clamp keeps, shortened so `mark` fits after it within
    /// `max_width` (non-positive for any width) and ending with `mark` as one more cluster.
    fn place_cut(
        &self,
        range: Range<usize>,
        mark: &ShapedParagraph<'_>,
        max_width: f32,
        (cluster_base, line): (usize, usize),
        spans: &mut Vec<ClusterSpan>,
    ) -> LaidLine {
        let mark_width = advance(&mark.clusters);
        let mut end = self.cut_end(range.clone(), mark_width, max_width);
        // Spaces before the cut go with the text after it.
        while end > range.start && self.clusters[end - 1].hangs {
            end -= 1;
        }
        self.push_spans(range.start..end, cluster_base, line, spans);
        let at = self
            .clusters
            .get(end)
            .map_or(self.text.len(), |next| next.start);
        spans.push(ClusterSpan {
            bytes: at..at,
            line,
            look: 0,
            ellipsis: true,
        });
        let mut laid = self.position(range.start..end, cluster_base);
        // The letter spacing of the last kept cluster, which no longer ends the line.
        let gap = self.clusters[range.start..end]
            .last()
            .map_or(0.0, |cluster| cluster.spacing);
        let mut glyphs = mark.position(0..mark.clusters.len(), 0).glyphs;
        let shift = if laid.rtl { 0.0 } else { laid.width + gap };
        for glyph in &mut glyphs {
            glyph.cluster = cluster_base + end;
            glyph.x += shift;
        }
        if laid.rtl {
            for glyph in &mut laid.glyphs {
                glyph.x += mark_width + gap;
            }
            glyphs.append(&mut laid.glyphs);
            laid.glyphs = glyphs;
        } else {
            laid.glyphs.append(&mut glyphs);
        }
        laid.width += mark_width + gap;
        laid.has_glyphs = true;
        laid
    }

    /// Where a line holding `range` is cut so `mark_width` more fits in `max_width`: the last
    /// soft wrap opportunity (or the paragraph's end) at which it fits, else the last cluster
    /// boundary, else the line's start.
    fn cut_end(&self, range: Range<usize>, mark_width: f32, max_width: f32) -> usize {
        if max_width <= 0.0 {
            return range.end;
        }
        let fits = |end: usize| {
            let kept = &self.clusters[range.start..end];
            let width = kept
                .iter()
                .rposition(|cluster| !cluster.hangs)
                .map_or(0.0, |last| {
                    kept[..=last]
                        .iter()
                        .fold(0.0, |width, cluster| width + cluster.width)
                });
            width + mark_width <= max_width
        };
        let soft = |end: usize| end == self.clusters.len() || self.clusters[end].break_before;
        let ends = || (range.start + 1..=range.end).rev();
        ends()
            .find(|&end| soft(end) && fits(end))
            .or_else(|| ends().find(|&end| fits(end)))
            .unwrap_or(range.start)
    }

    fn push_spans(
        &self,
        range: Range<usize>,
        cluster_base: usize,
        line: usize,
        spans: &mut Vec<ClusterSpan>,
    ) {
        debug_assert_eq!(spans.len(), cluster_base + range.start);
        for index in range {
            let end = self
                .clusters
                .get(index + 1)
                .map_or(self.text.len(), |next| next.start);
            spans.push(ClusterSpan {
                bytes: self.clusters[index].start..end.max(self.clusters[index].start),
                line,
                look: self.clusters[index].look,
                ellipsis: false,
            });
        }
    }

    /// The clusters of `range` in visual order, pens from the line's start. Trailing spaces
    /// hang: no width, nothing drawn.
    fn position(&self, range: Range<usize>, cluster_base: usize) -> LaidLine {
        let clusters = &self.clusters;
        let glyphs = &self.glyphs;
        let has_glyphs = !range.is_empty();
        let mut end = range.end;
        while end > range.start && clusters[end - 1].hangs {
            end -= 1;
        }
        let mut line = LaidLine {
            glyphs: Vec::new(),
            width: 0.0,
            rtl: self.bidi.paragraph_level.is_rtl(),
            has_glyphs,
            size: 0.0,
        };
        if end == range.start {
            return line;
        }
        let bytes = clusters[range.start].start
            ..clusters
                .get(range.end)
                .map_or(self.text.len(), |next| next.start);
        let levels = self.bidi.reordered_levels(bytes);
        // Letter spacing goes between visual neighbours, never after the line's last cluster.
        let mut cluster_of = vec![0; clusters[end - 1].glyphs.end];
        for (index, cluster) in clusters[range.start..end].iter().enumerate() {
            cluster_of[cluster.glyphs.clone()].fill(range.start + index);
        }
        let mut x = 0.0_f32;
        let mut previous: Option<usize> = None;
        for index in visual_order(
            clusters[range.start].glyphs.start..clusters[end - 1].glyphs.end,
            |index| levels[glyphs[index].cluster].number(),
        ) {
            let glyph = &glyphs[index];
            let cluster = cluster_of[index];
            if let Some(before) = previous.filter(|&before| before != cluster) {
                x += clusters[before].spacing;
            }
            previous = Some(cluster);
            line.glyphs.push(PositionedGlyph {
                font: glyph.font,
                glyph_index: glyph.glyph_index,
                cluster: cluster_base + cluster,
                advance: glyph.advance,
                x,
                x_offset: glyph.x_offset,
                y_offset: glyph.y_offset,
                size: glyph.size,
                shear: glyph.shear,
                look: clusters[cluster].look,
            });
            x += glyph.advance;
        }
        line.width = x;
        line
    }
}

/// Reorders logical-order glyphs visually per UAX #9 rule L2.
fn visual_order(range: Range<usize>, level: impl Fn(usize) -> u8) -> Vec<usize> {
    let mut order = range.collect::<Vec<_>>();
    let levels = order.iter().map(|&index| level(index)).collect::<Vec<_>>();
    let highest = levels.iter().copied().max().unwrap_or(0);
    let Some(lowest_odd) = levels.iter().copied().filter(|level| level % 2 == 1).min() else {
        return order;
    };
    let mut current = levels;
    for at_least in (lowest_odd..=highest).rev() {
        let mut start = 0;
        while start < order.len() {
            if current[start] < at_least {
                start += 1;
                continue;
            }
            let end = (start..order.len())
                .find(|&index| current[index] < at_least)
                .unwrap_or(order.len());
            order[start..end].reverse();
            current[start..end].reverse();
            start = end;
        }
    }
    order
}

/// A run with one direction, script and font.
#[derive(Clone, Debug, PartialEq)]
struct Run {
    range: Range<usize>,
    font: usize,
    script: Option<Script>,
    rtl: bool,
    segment: usize,
}

#[derive(Clone, Debug, PartialEq)]
struct ShapeSegment {
    range: Range<usize>,
    look: u16,
}

fn segments(
    paragraph: &str,
    shaping: &runs::Shaping,
    look_of: impl Fn(usize) -> u16,
) -> Vec<ShapeSegment> {
    let mut segments = Vec::<ShapeSegment>::new();
    for (start, grapheme) in paragraph.grapheme_indices(true) {
        let look = look_of(start);
        let end = start + grapheme.len();
        match segments.last_mut() {
            Some(segment)
                if shaping.looks[usize::from(segment.look)]
                    .shapes_like(&shaping.looks[usize::from(look)]) =>
            {
                segment.range.end = end;
            }
            _ => segments.push(ShapeSegment {
                range: start..end,
                look,
            }),
        }
    }
    segments
}

struct SegmentOrders<'o> {
    order: &'o [usize],
    color_first: Vec<usize>,
    text_first: Vec<usize>,
    color: bool,
    text: bool,
}

impl<'o> SegmentOrders<'o> {
    fn of(faces: &Faces<'_>, order: &'o [usize]) -> Self {
        let (color, text): (Vec<usize>, Vec<usize>) =
            order.iter().partition(|&&font| faces.color[font]);
        Self {
            order,
            color_first: color.iter().chain(&text).copied().collect(),
            text_first: text.iter().chain(&color).copied().collect(),
            color: !color.is_empty(),
            text: !text.is_empty(),
        }
    }
}

/// Splits a paragraph into runs.
///
/// Each grapheme takes the first font in its segment's order that covers it. Common-only
/// graphemes (spaces, digits, punctuation) stay in the previous font of their segment when it
/// covers them and take the neighbouring script. Uncovered graphemes take their base character's
/// font, else the previous font. Emoji prefer color fonts under [`EmojiPolicy::Color`] and text
/// fonts otherwise (as does U+FE0E).
fn itemize(
    faces: &Faces<'_>,
    paragraph: &str,
    bidi: &ParagraphBidiInfo<'_>,
    segments: &[ShapeSegment],
    orders: &[Vec<usize>],
    policy: EmojiPolicy,
) -> Vec<Run> {
    let prepared = segments
        .iter()
        .map(|segment| SegmentOrders::of(faces, &orders[usize::from(segment.look)]))
        .collect::<Vec<_>>();
    let mut graphemes = Vec::<(usize, usize, Option<Script>, bool, usize)>::new();
    let mut current = None;
    let mut segment = 0;
    for (start, grapheme) in paragraph.grapheme_indices(true) {
        while segment + 1 < segments.len() && start >= segments[segment].range.end {
            segment += 1;
            current = None;
        }
        let orders = &prepared[segment];
        let first = orders.order.first().copied().unwrap_or(0);
        let fits = |font: usize, color_wanted: bool| {
            faces.color[font] == color_wanted
                || if color_wanted {
                    !orders.color
                } else {
                    !orders.text
                }
        };
        let script = grapheme
            .chars()
            .map(|character| character.script())
            .find(|script| !matches!(script, Script::Common | Script::Inherited | Script::Unknown));
        let wants_color = if grapheme.contains('\u{fe0e}') {
            Some(false)
        } else if is_emoji(grapheme) {
            Some(policy == EmojiPolicy::Color)
        } else {
            None
        };
        let order = match wants_color {
            Some(true) => orders.color_first.as_slice(),
            Some(false) => orders.text_first.as_slice(),
            None => orders.order,
        };
        let stays = current.filter(|&font| {
            script.is_none()
                && wants_color.is_none_or(|wanted| fits(font, wanted))
                && faces.covers(font, grapheme)
        });
        let font = stays
            .or_else(|| {
                order
                    .iter()
                    .copied()
                    .find(|&font| faces.covers(font, grapheme))
            })
            .or_else(|| {
                let base = grapheme
                    .chars()
                    .find(|&character| !is_default_ignorable(character))?;
                order.iter().copied().find(|&font| faces.has(font, base))
            })
            .or(current)
            .unwrap_or(first);
        current = Some(font);
        graphemes.push((start, font, script, bidi.levels[start].is_rtl(), segment));
    }
    let leading = graphemes.iter().find_map(|grapheme| grapheme.2);
    let mut before = leading;
    for grapheme in &mut graphemes {
        match grapheme.2 {
            Some(script) => before = Some(script),
            None => grapheme.2 = before,
        }
    }
    let mut runs = Vec::<Run>::new();
    for (index, &(start, font, script, rtl, segment)) in graphemes.iter().enumerate() {
        let end = graphemes
            .get(index + 1)
            .map_or(paragraph.len(), |next| next.0);
        match runs.last_mut() {
            Some(run)
                if run.font == font
                    && run.script == script
                    && run.rtl == rtl
                    && run.segment == segment =>
            {
                run.range.end = end;
            }
            _ => runs.push(Run {
                range: start..end,
                font,
                script,
                rtl,
                segment,
            }),
        }
    }
    runs
}

fn shaping_script(script: Script) -> Option<rustybuzz::Script> {
    let code: [u8; 4] = script.short_name().as_bytes().try_into().ok()?;
    rustybuzz::Script::from_iso15924_tag(ttf_parser::Tag::from_bytes(&code))
}

/// Shapes each run with surrounding text as context, returning glyphs in logical order.
fn shape_paragraph(
    faces: &Faces<'_>,
    paragraph: &str,
    bidi: &ParagraphBidiInfo<'_>,
    shaping: &runs::Shaping,
    segments: &[ShapeSegment],
    policy: EmojiPolicy,
) -> Vec<ShapedGlyph> {
    let mut glyphs = Vec::new();
    for run in itemize(faces, paragraph, bidi, segments, &shaping.orders, policy) {
        let look = &shaping.looks[usize::from(segments[run.segment].look)];
        let font_size = look.size;
        // CSS Text 3: letter spacing disables optional ligatures only.
        let features = if look.spacing == 0.0 {
            Vec::new()
        } else {
            [b"liga", b"clig", b"dlig", b"hlig"]
                .map(|tag| rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(tag), 0, ..))
                .to_vec()
        };
        let shear = look.style != FontStyle::Normal && !faces.slanted(run.font);
        let face = faces.shaper(run.font);
        let units = faces.units_per_em(run.font);
        let mut buffer = UnicodeBuffer::new();
        buffer.push_str(&paragraph[run.range.clone()]);
        buffer.set_pre_context(&paragraph[..run.range.start]);
        buffer.set_post_context(&paragraph[run.range.end..]);
        buffer.set_direction(if run.rtl {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        });
        match run.script.and_then(shaping_script) {
            Some(script) => buffer.set_script(script),
            None => buffer.guess_segment_properties(),
        }
        let shaped = rustybuzz::shape(face, &features, buffer);
        let first = glyphs.len();
        for (info, position) in shaped.glyph_infos().iter().zip(shaped.glyph_positions()) {
            #[allow(clippy::cast_precision_loss)]
            glyphs.push(ShapedGlyph {
                font: run.font,
                // AAT `morx` substitutions may name a missing glyph; draw it as `.notdef`.
                glyph_index: u16::try_from(info.glyph_id)
                    .ok()
                    .filter(|&glyph| glyph < face.number_of_glyphs())
                    .unwrap_or(0),
                cluster: run.range.start + usize::try_from(info.cluster).unwrap_or(0),
                advance: position.x_advance as f32 * font_size / units,
                x_offset: position.x_offset as f32 * font_size / units,
                y_offset: position.y_offset as f32 * font_size / units,
                size: font_size,
                shear,
            });
        }
        // RTL runs come back in visual order; line breaking needs logical order.
        if run.rtl {
            glyphs[first..].reverse();
        }
    }
    glyphs
}

#[derive(Copy, Clone, Debug)]
struct ShapedGlyph {
    font: usize,
    glyph_index: u16,
    /// Byte offset in the paragraph.
    cluster: usize,
    advance: f32,
    /// Shaper offset from the pen position in pixels, y up.
    x_offset: f32,
    y_offset: f32,
    size: f32,
    shear: bool,
}

/// Glyphs a line never splits.
#[derive(Clone, Debug, PartialEq)]
struct Cluster {
    glyphs: Range<usize>,
    /// Byte offset in the paragraph.
    start: usize,
    /// Advance including letter spacing.
    width: f32,
    /// Letter spacing included in `width`.
    spacing: f32,
    /// Only hanging spaces.
    hangs: bool,
    /// UAX #14 allows a break before it.
    break_before: bool,
    size: f32,
    look: u16,
}

/// Groups glyphs into clusters with UAX #14 break opportunities, tailored so Korean breaks
/// between words rather than between Hangul characters. Cursive clusters get no letter spacing.
fn clusters(
    glyphs: &[ShapedGlyph],
    paragraph: &str,
    shaping: &runs::Shaping,
    look_of: impl Fn(usize) -> u16,
) -> Vec<Cluster> {
    let mut opportunities = unicode_linebreak::linebreaks(paragraph)
        .filter(|&(at, kind)| {
            kind == BreakOpportunity::Allowed
                && !(paragraph[..at].chars().next_back().is_some_and(is_hangul)
                    && paragraph[at..].chars().next().is_some_and(is_hangul))
        })
        .map(|(at, _)| at)
        .peekable();
    let mut clusters = Vec::<Cluster>::new();
    for (index, glyph) in glyphs.iter().enumerate() {
        match clusters.last_mut() {
            Some(cluster) if cluster.start == glyph.cluster => {
                cluster.glyphs.end = index + 1;
                cluster.width += glyph.advance;
            }
            _ => clusters.push(Cluster {
                glyphs: index..index + 1,
                start: glyph.cluster,
                width: glyph.advance,
                spacing: 0.0,
                hangs: false,
                break_before: false,
                size: glyph.size,
                look: look_of(glyph.cluster),
            }),
        }
    }
    for index in 0..clusters.len() {
        let start = clusters[index].start;
        let end = clusters
            .get(index + 1)
            .map(|next| next.start)
            .filter(|&end| end > start)
            .unwrap_or_else(|| {
                paragraph[start..]
                    .chars()
                    .next()
                    .map_or(start, |character| start + character.len_utf8())
            });
        clusters[index].hangs = paragraph[start..end].chars().all(hangs);
        let letter_spacing = shaping.looks[usize::from(clusters[index].look)].spacing;
        if letter_spacing != 0.0 && !is_cursive(&paragraph[start..end]) {
            clusters[index].spacing = letter_spacing;
            clusters[index].width += letter_spacing;
        }
        let mut found = false;
        while let Some(&at) = opportunities.peek() {
            if at > start {
                break;
            }
            opportunities.next();
            found = true;
        }
        clusters[index].break_before = index > 0 && found;
    }
    clusters
}

/// Breaks clusters into lines no wider than `max_width` (non-positive does not wrap) at the
/// last fitting opportunity, else between clusters. Hanging spaces never overflow a line.
fn break_lines(clusters: &[Cluster], max_width: f32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut x = 0.0_f32;
    // Latest break opportunity and the pen position there.
    let mut opportunity: Option<(usize, f32)> = None;
    for (index, cluster) in clusters.iter().enumerate() {
        if cluster.break_before && index > start {
            opportunity = Some((index, x));
        }
        let fits = |x: f32| x + cluster.width - cluster.spacing <= max_width;
        if max_width > 0.0 && !cluster.hangs && index > start && !fits(x) {
            let (end, width) = opportunity.unwrap_or((index, x));
            lines.push(start..end);
            start = end;
            x -= width;
            opportunity = None;
            // The carried-over text is still too wide, so break again before this cluster.
            if index > start && !fits(x) {
                lines.push(start..index);
                start = index;
                x = 0.0;
            }
        }
        x += cluster.width;
    }
    lines.push(start..clusters.len());
    lines
}

#[derive(Copy, Clone, Debug)]
struct PositionedGlyph {
    font: usize,
    glyph_index: u16,
    /// Cluster index in logical order.
    cluster: usize,
    /// Excludes letter spacing.
    advance: f32,
    /// Pen position from the line's start.
    x: f32,
    /// Shaper offset from the pen position in pixels, y up.
    x_offset: f32,
    y_offset: f32,
    size: f32,
    shear: bool,
    look: u16,
}

#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
struct DrawingKey {
    face: usize,
    glyph: u16,
    shear: bool,
    size: u32,
    foreground: [u8; 4],
}

fn drawing_key(shaping: &runs::Shaping, glyph: &PositionedGlyph) -> DrawingKey {
    DrawingKey {
        face: glyph.font,
        glyph: glyph.glyph_index,
        shear: glyph.shear,
        size: glyph.size.to_bits(),
        foreground: shaping.looks[usize::from(glyph.look)].fill.foreground(),
    }
}

/// The synthetic oblique shear in font units, y up: `x' = x + 0.25 y`, 14.04 degrees, CSS
/// Fonts 4's default oblique angle of 14 degrees within 0.05.
const OBLIQUE_SHEAR: f32 = 0.25;

/// An outline filled with the text paint, or under [`EmojiPolicy::Color`] a color form.
enum Drawing {
    Outline(Outline),
    Painting(color::Painting),
    Bitmap(color::Bitmap),
}

impl Drawing {
    /// COLR painting, else bitmap, else outline. The painting uses the text color opaque; its
    /// alpha applies to the whole glyph at composition.
    fn of(faces: &Faces<'_>, key: DrawingKey, policy: EmojiPolicy) -> Self {
        let face = &faces.parsed[key.face];
        if policy == EmojiPolicy::Color && faces.color[key.face] {
            let [red, green, blue, _] = key.foreground;
            let shear = if key.shear { OBLIQUE_SHEAR } else { 0.0 };
            if let Some(painting) =
                color::Painting::of(face, key.glyph, [red, green, blue, 255], shear)
            {
                return Self::Painting(painting);
            }
            if let Some(bitmap) = color::Bitmap::of(face, key.glyph, f32::from_bits(key.size)) {
                return Self::Bitmap(bitmap);
            }
        }
        let outline = Outline::of(face, key.glyph);
        Self::Outline(if key.shear {
            outline.sheared(OBLIQUE_SHEAR)
        } else {
            outline
        })
    }

    /// Conservative `[x_min, y_min, x_max, y_max]` in font units.
    fn bounds(&self) -> Option<[f32; 4]> {
        match self {
            Self::Outline(outline) => outline.bounds,
            Self::Painting(painting) => painting.bounds(),
            Self::Bitmap(bitmap) => Some(bitmap.bounds()),
        }
    }

    /// Tight `[x_min, y_min, x_max, y_max]` in font units of what is actually drawn.
    fn ink_bounds(&self) -> Option<[f32; 4]> {
        match self {
            Self::Outline(outline) => outline.curve_bounds(),
            Self::Painting(painting) => painting.bounds(),
            Self::Bitmap(bitmap) => bitmap.ink_bounds(),
        }
    }

    /// Premultiplied RGBA8; empty for an outline.
    fn draw_color(&self, origin: [f32; 2], scale: f32, pixels: PixelBox) -> Vec<u8> {
        match self {
            Self::Outline(_) => Vec::new(),
            Self::Painting(painting) => painting.draw(origin, scale, pixels),
            Self::Bitmap(bitmap) => bitmap.draw(origin, scale, pixels),
        }
    }
}

/// A glyph outline in font units, y up.
#[derive(Default)]
struct Outline {
    segments: Vec<Segment>,
    /// Bounds of all points, control points included.
    bounds: Option<[f32; 4]>,
}

enum Segment {
    Line([f32; 2], [f32; 2]),
    Quad([f32; 2], [f32; 2], [f32; 2]),
    Cubic([f32; 2], [f32; 2], [f32; 2], [f32; 2]),
}

/// Pixel box in the layout box, right and bottom exclusive.
#[derive(Copy, Clone, Debug)]
struct PixelBox {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

impl PixelBox {
    #[allow(clippy::cast_possible_truncation)]
    fn of(bounds: [f32; 4], origin: [f32; 2], scale: f32) -> Self {
        let [x_min, y_min, x_max, y_max] = bounds;
        // Same expression as `Outline::draw`, so every point lands inside.
        Self {
            left: (origin[0] + x_min * scale).floor() as i64,
            top: (origin[1] - y_max * scale).floor() as i64,
            right: (origin[0] + x_max * scale).ceil() as i64,
            bottom: (origin[1] - y_min * scale).ceil() as i64,
        }
    }
}

#[cfg(test)]
thread_local! {
    // Already `const`; clippy misreads the emulated-TLS expansion used on Android.
    #[allow(clippy::missing_const_for_thread_local)]
    static OUTLINES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Outline {
    fn of(face: &ttf_parser::Face<'_>, glyph: u16) -> Self {
        #[cfg(test)]
        OUTLINES_READ.with(|read| read.set(read.get() + 1));
        let mut sink = OutlineSink::default();
        face.outline_glyph(GlyphId(glyph), &mut sink);
        sink.close();
        if sink.overflow {
            return Self::default();
        }
        sink.outline
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    fn draw(&self, origin: [f32; 2], scale: f32, pixels: PixelBox) -> Vec<u8> {
        let width = usize::try_from(pixels.right - pixels.left).unwrap_or(0);
        let height = usize::try_from(pixels.bottom - pixels.top).unwrap_or(0);
        let (left, top) = (pixels.left as f32, pixels.top as f32);
        let raster = self.raster(width, height, |[x, y]| {
            point(origin[0] + x * scale - left, (origin[1] - y * scale) - top)
        });
        let mut alpha = vec![0_u8; width * height];
        raster.for_each_pixel(|index, coverage| {
            alpha[index] = (coverage.min(1.0) * 255.0).round() as u8;
        });
        alpha
    }
}

impl Outline {
    fn sheared(self, shear: f32) -> Self {
        let map = |[x, y]: [f32; 2]| [x + shear * y, y];
        let segments = self
            .segments
            .into_iter()
            .map(|segment| match segment {
                Segment::Line(from, to) => Segment::Line(map(from), map(to)),
                Segment::Quad(from, control, to) => Segment::Quad(map(from), map(control), map(to)),
                Segment::Cubic(from, first, second, to) => {
                    Segment::Cubic(map(from), map(first), map(second), map(to))
                }
            })
            .collect::<Vec<_>>();
        let mut bounds = None;
        for segment in &segments {
            let points: &[[f32; 2]] = match segment {
                Segment::Line(from, to) => &[*from, *to],
                Segment::Quad(from, control, to) => &[*from, *control, *to],
                Segment::Cubic(from, first, second, to) => &[*from, *first, *second, *to],
            };
            for &[x, y] in points {
                include(&mut bounds, [x, y, x, y]);
            }
        }
        Self { segments, bounds }
    }

    /// Bounds of the curves themselves (endpoints and extrema), tighter than `bounds`.
    fn curve_bounds(&self) -> Option<[f32; 4]> {
        let mut bounds = None;
        let mut hold = |[x, y]: [f32; 2]| include(&mut bounds, [x, y, x, y]);
        for segment in &self.segments {
            match *segment {
                Segment::Line(from, to) => {
                    hold(from);
                    hold(to);
                }
                Segment::Quad(from, control, to) => {
                    hold(from);
                    hold(to);
                    for axis in 0..2 {
                        let curvature = from[axis] - 2.0 * control[axis] + to[axis];
                        let t = (from[axis] - control[axis]) / curvature;
                        if t > 0.0 && t < 1.0 {
                            hold(std::array::from_fn(|axis| {
                                let u = 1.0 - t;
                                u * u * from[axis] + 2.0 * u * t * control[axis] + t * t * to[axis]
                            }));
                        }
                    }
                }
                Segment::Cubic(from, first, second, to) => {
                    hold(from);
                    hold(to);
                    for axis in 0..2 {
                        let [p0, p1, p2, p3] = [from[axis], first[axis], second[axis], to[axis]];
                        for t in quadratic_roots(
                            -p0 + 3.0 * (p1 - p2) + p3,
                            2.0 * (p0 - 2.0 * p1 + p2),
                            p1 - p0,
                        )
                        .into_iter()
                        .flatten()
                        .filter(|t| *t > 0.0 && *t < 1.0)
                        {
                            hold(std::array::from_fn(|axis| {
                                let u = 1.0 - t;
                                u * u * u * from[axis]
                                    + 3.0 * u * u * t * first[axis]
                                    + 3.0 * u * t * t * second[axis]
                                    + t * t * t * to[axis]
                            }));
                        }
                    }
                }
            }
        }
        bounds
    }

    fn raster(&self, width: usize, height: usize, at: impl Fn([f32; 2]) -> Point) -> Rasterizer {
        let mut raster = Rasterizer::new(width, height);
        for segment in &self.segments {
            match *segment {
                Segment::Line(from, to) => raster.draw_line(at(from), at(to)),
                Segment::Quad(from, control, to) => raster.draw_quad(at(from), at(control), at(to)),
                Segment::Cubic(from, first, second, to) => {
                    raster.draw_cubic(at(from), at(first), at(second), at(to));
                }
            }
        }
        raster
    }
}

/// Real roots of `a t^2 + b t + c`; non-finite results count as none.
fn quadratic_roots(a: f32, b: f32, c: f32) -> [Option<f32>; 2] {
    let finite = |t: f32| t.is_finite().then_some(t);
    if a.abs() <= f32::EPSILON * (b.abs() + c.abs()) {
        return [finite(-c / b), None];
    }
    let discriminant = b * b - 4.0 * a * c;
    if discriminant < 0.0 {
        return [None, None];
    }
    let root = discriminant.sqrt();
    [
        finite((-b + root) / (2.0 * a)),
        finite((-b - root) / (2.0 * a)),
    ]
}

/// Most segments one glyph outline may hold; a glyph past it draws nothing.
const MAX_GLYPH_SEGMENTS: usize = 65_536;

#[derive(Default)]
struct OutlineSink {
    outline: Outline,
    start: [f32; 2],
    pen: [f32; 2],
    overflow: bool,
}

impl OutlineSink {
    /// Whether another segment fits; the font parser cannot be stopped, so past the limit the
    /// sink only stops recording.
    fn room(&mut self) -> bool {
        self.overflow |= self.outline.segments.len() >= MAX_GLYPH_SEGMENTS;
        !self.overflow
    }

    fn include(&mut self, points: &[[f32; 2]]) {
        for &[x, y] in points {
            let bounds = self.outline.bounds.get_or_insert([x, y, x, y]);
            *bounds = [
                bounds[0].min(x),
                bounds[1].min(y),
                bounds[2].max(x),
                bounds[3].max(y),
            ];
        }
    }
}

impl OutlineBuilder for OutlineSink {
    fn move_to(&mut self, x: f32, y: f32) {
        self.close();
        self.start = [x, y];
        self.pen = [x, y];
    }

    fn line_to(&mut self, x: f32, y: f32) {
        if !self.room() {
            return;
        }
        let to = [x, y];
        self.include(&[self.pen, to]);
        self.outline.segments.push(Segment::Line(self.pen, to));
        self.pen = to;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        if !self.room() {
            return;
        }
        let (control, to) = ([x1, y1], [x, y]);
        self.include(&[self.pen, control, to]);
        self.outline
            .segments
            .push(Segment::Quad(self.pen, control, to));
        self.pen = to;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        if !self.room() {
            return;
        }
        let (first, second, to) = ([x1, y1], [x2, y2], [x, y]);
        self.include(&[self.pen, first, second, to]);
        self.outline
            .segments
            .push(Segment::Cubic(self.pen, first, second, to));
        self.pen = to;
    }

    fn close(&mut self) {
        // A contour already back at its start needs no closing line.
        if self.pen.map(f32::to_bits) != self.start.map(f32::to_bits) {
            self.line_to(self.start[0], self.start[1]);
        }
    }
}

fn validate_request(request: &TextRasterRequest) -> Result<(), TextRasterError> {
    if request.font_size.is_finite()
        && request.font_size > 0.0
        && request.font_size <= MAX_FONT_SIZE
        && request.text.len() <= MAX_TEXT_BYTES
        && request.max_width.is_finite()
        && request.max_width <= MAX_TEXT_BOX_PIXELS
        && request.line_height.is_finite()
        && request.line_height > 0.0
        && request.line_height <= MAX_LINE_HEIGHT
        && request.letter_spacing.is_finite()
        && request.letter_spacing >= LETTER_SPACING_RANGE.start() * request.font_size
        && request.letter_spacing <= LETTER_SPACING_RANGE.end() * request.font_size
        && (1..=1000).contains(&request.font_weight)
        && request.content_version > 0
        && request.fill.is_valid()
        && valid_strokes(&request.strokes)
        && request
            .background
            .as_ref()
            .is_none_or(TextBackground::is_valid)
        && request.clamp.as_ref().is_none_or(|clamp| {
            clamp.max_lines != Some(0)
                && clamp.max_height.is_finite()
                && clamp.ellipsis.as_ref().is_none_or(|ellipsis| {
                    ellipsis.len() <= MAX_ELLIPSIS_BYTES && !ellipsis.contains('\n')
                })
        })
        && runs::valid_spans(request)
        && (1.0..=MAX_CLUSTER_SCALE).contains(&request.advance_headroom)
        && request.highlight.as_ref().is_none_or(|highlight| {
            highlight.look.is_valid()
                && highlight.steps.len() <= MAX_HIGHLIGHT_STEPS
                && highlight.steps.iter().all(|(bytes, progress)| {
                    bytes.start <= bytes.end
                        && bytes.end <= request.text.len()
                        && (0.0..=1.0).contains(progress)
                })
        })
        && request
            .animator_pivot
            .iter()
            .all(|fraction| (0.0..=1.0).contains(fraction))
    {
        Ok(())
    } else {
        Err(TextRasterError::InvalidRequest)
    }
}

fn valid_strokes(strokes: &[TextStroke]) -> bool {
    strokes.len() <= MAX_STROKES
        && strokes
            .iter()
            .all(|stroke| stroke.width.is_finite() && stroke.width > 0.0)
        && stroke_radius(strokes).is_none_or(|radius| radius <= MAX_STROKE_EXTENT)
}

fn checked_dimension(value: f32) -> Result<u32, TextRasterError> {
    if value.is_finite() && value > 0.0 && value.ceil() <= MAX_TEXT_BOX_PIXELS {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(value.ceil() as u32)
    } else {
        Err(TextRasterError::TooLarge)
    }
}

/// Max-combines a glyph's coverage into the mask so overlapping glyphs never erase each other.
fn place_glyph(
    mask: &mut [u8],
    width: u32,
    height: u32,
    origin_x: i64,
    origin_y: i64,
    glyph_width: usize,
    alpha: &[u8],
) {
    for (index, coverage) in alpha.iter().copied().enumerate() {
        let x = origin_x + i64::try_from(index % glyph_width.max(1)).unwrap_or(0);
        let y = origin_y + i64::try_from(index / glyph_width.max(1)).unwrap_or(0);
        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
            continue;
        }
        let pixel =
            u64::try_from(y).unwrap_or(0) * u64::from(width) + u64::try_from(x).unwrap_or(0);
        let slot = &mut mask[usize::try_from(pixel).unwrap_or(0)];
        *slot = (*slot).max(coverage);
    }
}

/// Composites a color glyph over the color layer; later glyphs go on top.
fn place_color_glyph(
    colors: &mut [u8],
    width: u32,
    height: u32,
    origin_x: i64,
    origin_y: i64,
    glyph_width: usize,
    rgba8: &[u8],
) {
    for (index, source) in rgba8.as_chunks::<4>().0.iter().enumerate() {
        let x = origin_x + i64::try_from(index % glyph_width.max(1)).unwrap_or(0);
        let y = origin_y + i64::try_from(index / glyph_width.max(1)).unwrap_or(0);
        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) || source[3] == 0 {
            continue;
        }
        let pixel =
            u64::try_from(y).unwrap_or(0) * u64::from(width) + u64::try_from(x).unwrap_or(0);
        let slot = &mut colors[usize::try_from(pixel).unwrap_or(0) * 4..][..4];
        let blended = over(
            std::array::from_fn(|channel| u32::from(source[channel])),
            std::array::from_fn(|channel| u32::from(slot[channel])),
        );
        for (byte, value) in slot.iter_mut().zip(blended) {
            *byte = u8::try_from(value.min(255)).unwrap_or(u8::MAX);
        }
    }
}

impl TextRasterRequest {
    /// Whole pixels the strokes or background reach past the layout box, horizontally and
    /// vertically. The texture margin ([`TextTexture::layout_x`]) adds room for ink overflow to
    /// this.
    #[must_use]
    pub fn decoration_margin(&self) -> [u32; 2] {
        let (outline, padding) = decoration_extents(self);
        padding.map(|padding| outline.max(padding))
    }
}

/// Whole-pixel reach of the outermost stroke past the ink, and of the background past the box
/// horizontally and vertically.
fn decoration_extents(request: &TextRasterRequest) -> (u32, [u32; 2]) {
    let outline = stroke_radius(&request.strokes).map_or(0, disc_extent);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let padding = request
        .background
        .iter()
        .chain(request.highlight.as_ref().map(|highlight| &highlight.look))
        .fold([0; 2], |held, background| {
            let [top, right, bottom, left] = background.padding;
            let reach = [right.max(left), top.max(bottom)].map(|side| side.ceil() as u32);
            [held[0].max(reach[0]), held[1].max(reach[1])]
        });
    (outline, padding)
}

/// Sum of stroke widths, in order; `None` without strokes.
fn stroke_radius(strokes: &[TextStroke]) -> Option<f32> {
    let mut radius = None;
    for stroke in strokes {
        radius = Some(radius.unwrap_or(0.0) + stroke.width);
    }
    radius
}

/// Integer radius of the disc a band of radius `width` dilates by.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn disc_extent(width: f32) -> u32 {
    (width + 0.5).ceil() as u32
}

/// Premultiplies `color` by `coverage`, truncating each product to a byte. Pinned rasters
/// depend on this truncation.
fn premultiplied(color: [u8; 4], coverage: u8) -> [u32; 4] {
    let alpha = u32::from(coverage) * u32::from(color[3]) / 255;
    [
        u32::from(color[0]) * alpha / 255,
        u32::from(color[1]) * alpha / 255,
        u32::from(color[2]) * alpha / 255,
        alpha,
    ]
}

fn over(source: [u32; 4], destination: [u32; 4]) -> [u32; 4] {
    let keep = 255 - source[3];
    std::array::from_fn(|channel| source[channel] + (destination[channel] * keep + 127) / 255)
}

trait PixelLook {
    /// Straight sRGB fill at `index`; `point` is the pixel center in the layout box.
    fn fill(&self, index: usize, point: [f32; 2]) -> [u8; 4];
    fn band(&self, stroke: usize, index: usize) -> [u8; 4];
    /// 8-bit opacity applied to the glyph layers together; `None` for opaque.
    fn fade(&self, _index: usize) -> Option<u32> {
        None
    }
    fn color_alpha(&self, index: usize) -> u32;
}

struct RequestLook<'a> {
    foreground: [u8; 4],
    gradient: Option<gradient::PreparedGradient<'a>>,
    strokes: &'a [TextStroke],
}

impl<'a> RequestLook<'a> {
    fn of(request: &'a TextRasterRequest, layout_size: [f32; 2]) -> Self {
        Self::painting(&request.fill, &request.strokes, layout_size)
    }

    fn painting(fill: &'a TextFill, strokes: &'a [TextStroke], layout_size: [f32; 2]) -> Self {
        Self {
            foreground: fill.foreground(),
            gradient: match fill {
                TextFill::Solid(_) => None,
                TextFill::Gradient(gradient) => Some(gradient.prepared(layout_size)),
            },
            strokes,
        }
    }
}

impl PixelLook for RequestLook<'_> {
    #[inline]
    fn fill(&self, _: usize, point: [f32; 2]) -> [u8; 4] {
        self.gradient
            .as_ref()
            .map_or(self.foreground, |gradient| gradient.rgba8_at(point))
    }

    #[inline]
    fn band(&self, stroke: usize, _: usize) -> [u8; 4] {
        self.strokes
            .get(stroke)
            .map_or([0; 4], |stroke| stroke.color)
    }

    fn color_alpha(&self, _: usize) -> u32 {
        u32::from(self.foreground[3])
    }
}

/// Dilates coverage (including color glyph alpha) once per stroke, by the cumulative width.
fn stroke_bands(
    request: &TextRasterRequest,
    coverage: &[u8],
    colors: Option<&[u8]>,
    width: u32,
) -> Vec<Vec<u8>> {
    if request.strokes.is_empty() {
        return Vec::new();
    }
    let height = u32::try_from(coverage.len() / width.max(1) as usize).unwrap_or(0);
    let ink = colors.map(|colors| {
        coverage
            .iter()
            .zip(colors.as_chunks::<4>().0)
            .map(|(&text, color)| text.max(color[3]))
            .collect::<Vec<_>>()
    });
    let ink = ink.as_deref().unwrap_or(coverage);
    let mut radius = 0.0;
    request
        .strokes
        .iter()
        .map(|stroke| {
            radius += stroke.width;
            dilate(ink, width, height, radius)
        })
        .collect()
}

/// Boxes in raster pixels with the look they are painted in.
type BoxLayer<'a> = (&'a TextBackground, Vec<boxes::LineBox>);

/// The background boxes and the highlight boxes of `request`, padded and moved by `margin` into
/// raster pixels, each with its look.
#[expect(clippy::cast_precision_loss, reason = "margins are far below 2^24")]
fn box_layers<'a>(
    request: &'a TextRasterRequest,
    margin: [u32; 2],
    line_boxes: &[(f32, f32, f32, f32)],
    highlight: &[[f32; 4]],
) -> (Option<BoxLayer<'a>>, Option<BoxLayer<'a>>) {
    let [margin_x, margin_y] = margin.map(|margin| margin as f32);
    let background = request.background.as_ref().map(|background| {
        let boxes = line_boxes
            .iter()
            .map(|&(left, width, top, height)| {
                boxes::LineBox::padded(
                    background,
                    [
                        margin_x + left,
                        margin_x + left + width,
                        margin_y + top,
                        margin_y + top + height,
                    ],
                )
            })
            .collect::<Vec<_>>();
        (background, boxes)
    });
    let highlight = request
        .highlight
        .as_ref()
        .filter(|_| !highlight.is_empty())
        .map(|request| {
            let boxes = highlight
                .iter()
                .map(|&[left, right, top, bottom]| {
                    boxes::LineBox::padded(
                        &request.look,
                        [
                            margin_x + left,
                            margin_x + right,
                            margin_y + top,
                            margin_y + bottom,
                        ],
                    )
                })
                .collect::<Vec<_>>();
            (&request.look, boxes)
        });
    (background, highlight)
}

/// Composes text over strokes over line boxes into premultiplied RGBA8, in integers except for
/// gradient colors so every target matches. `bands` are innermost first and drawn outermost
/// first. With a split background the boxes are returned as their own plane.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::too_many_arguments
)]
fn compose(
    request: &TextRasterRequest,
    coverage: &[u8],
    colors: Option<&[u8]>,
    width: u32,
    margin: [u32; 2],
    line_boxes: &[(f32, f32, f32, f32)],
    run_boxes: &[paint::RunBox],
    highlight: &[[f32; 4]],
    bands: &[Vec<u8>],
    look: &impl PixelLook,
) -> (Vec<u8>, Option<Vec<u8>>) {
    let (background, highlight) = box_layers(request, margin, line_boxes, highlight);
    let split = request.split_background
        && (background.is_some() || !run_boxes.is_empty() || highlight.is_some());
    let mut split_plane = split.then(|| vec![0_u8; coverage.len() * 4]);
    let [margin_x, margin_y] = margin.map(|margin| margin as f32);
    let mut row_runs = Vec::<&paint::RunBox>::new();
    let mut rgba8 = vec![0_u8; coverage.len() * 4];
    // Boxes are in line order, so the window `first..last` only moves down.
    let (mut first, mut last) = (0, 0);
    for (index, text) in coverage.iter().copied().enumerate() {
        let mut pixel = [0_u32; 4];
        let x = (index % width as usize) as f32;
        let y = (index / width as usize) as f32;
        if !run_boxes.is_empty() && index % width as usize == 0 {
            row_runs.clear();
            row_runs.extend(
                run_boxes
                    .iter()
                    .filter(|run| margin_y + run.top < y + 1.0 && margin_y + run.bottom > y),
            );
        }
        if let Some((background, boxes)) = &background {
            if index % width as usize == 0 {
                while first < boxes.len() && boxes[first].bottom <= y {
                    first += 1;
                }
                last = last.max(first);
                while last < boxes.len() && boxes[last].top < y + 1.0 {
                    last += 1;
                }
            }
            pixel = boxes::paint(background, &boxes[first..last], [x, y]);
        }
        if let Some(run) = run_cover(&row_runs, [margin_x, margin_y], [x, y]) {
            pixel = over(run, pixel);
        }
        if let Some((look, boxes)) = &highlight {
            let lit = boxes::paint(look, boxes, [x, y]);
            if lit[3] > 0 {
                pixel = over(lit, pixel);
            }
        }
        if let Some(plane) = &mut split_plane {
            for (channel, value) in pixel.iter().enumerate() {
                plane[index * 4 + channel] = (*value).min(255) as u8;
            }
            pixel = [0; 4];
        }
        // Fade a character's layers together so its strokes never show through its fill.
        let fade = look.fade(index);
        let boxes = pixel;
        if fade.is_some() {
            pixel = [0; 4];
        }
        for (stroke, band) in bands.iter().enumerate().rev() {
            if band[index] > 0 {
                pixel = over(premultiplied(look.band(stroke, index), band[index]), pixel);
            }
        }
        if text > 0 {
            let fill = look.fill(index, [x + 0.5 - margin_x, y + 0.5 - margin_y]);
            pixel = over(premultiplied(fill, text), pixel);
        }
        if let Some(colors) = colors {
            let color = &colors[index * 4..][..4];
            if color[3] > 0 {
                let opacity = look.color_alpha(index);
                pixel = over(
                    std::array::from_fn(|channel| u32::from(color[channel]) * opacity / 255),
                    pixel,
                );
            }
        }
        if let Some(fade) = fade {
            pixel = over(pixel.map(|value| (value * fade + 127) / 255), boxes);
        }
        for (channel, value) in pixel.iter().enumerate() {
            rgba8[index * 4 + channel] = (*value).min(255) as u8;
        }
    }
    (rgba8, split_plane)
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "coverage in [0, 1] to a byte"
)]
fn run_cover(
    runs: &[&paint::RunBox],
    [margin_x, margin_y]: [f32; 2],
    [x, y]: [f32; 2],
) -> Option<[u32; 4]> {
    let (mut best, mut color) = (0.0_f32, [0; 4]);
    for run in runs {
        let across =
            ((margin_x + run.right).min(x + 1.0) - (margin_x + run.left).max(x)).clamp(0.0, 1.0);
        let down =
            ((margin_y + run.bottom).min(y + 1.0) - (margin_y + run.top).max(y)).clamp(0.0, 1.0);
        let covered = across * down;
        if covered > 0.0 && covered >= best {
            (best, color) = (covered, run.color);
        }
    }
    (best > 0.0).then(|| premultiplied(color, (best * 255.0).round() as u8))
}

/// Dilates coverage by a disc of radius `width` (max over the disc, rim pixels weighted).
///
/// Only the ink bounds grown by the disc are visited. Each disc row's fully weighted taps form one
/// run, computed as a van Herk/Gil-Werman sliding maximum; only the rim is visited tap by tap. The
/// result is byte-identical to a tap-by-tap pass.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap
)]
fn dilate(coverage: &[u8], width: u32, height: u32, radius: f32) -> Vec<u8> {
    let (width, height) = (width as usize, height as usize);
    let mut band = vec![0_u8; coverage.len()];
    let Some(ink) = ink_bounds(coverage, width, height) else {
        return band;
    };
    let disc = Disc::new(radius);
    let extent = disc.extent;
    let reach = Bounds {
        left: ink.left.saturating_sub(extent),
        top: ink.top.saturating_sub(extent),
        right: (ink.right + extent).min(width),
        bottom: (ink.bottom + extent).min(height),
    };
    // Compute each run half-width once per source row and apply it to every output row using it.
    let span = reach.right - reach.left;
    let mut window = WindowMax::default();
    let mut maxima = vec![0_u8; span];
    for (half, offsets) in &disc.runs {
        for source_y in ink.top..ink.bottom {
            let row = &coverage[source_y * width..(source_y + 1) * width];
            window.of(row, reach.left, span, *half, &mut maxima);
            for &offset in offsets {
                let Some(y) = source_y.checked_add_signed(-offset) else {
                    continue;
                };
                if y < reach.top || y >= reach.bottom {
                    continue;
                }
                let out = &mut band[y * width + reach.left..y * width + reach.right];
                for (pixel, highest) in out.iter_mut().zip(&maxima) {
                    *pixel = (*pixel).max(*highest);
                }
            }
        }
    }
    for &(dx, dy, weight) in &disc.rim {
        for source_y in ink.top..ink.bottom {
            let Some(y) = source_y.checked_add_signed(-dy) else {
                continue;
            };
            if y >= height {
                continue;
            }
            // Column x reads x + dx; keep both in the texture and the source in the ink.
            let first = ink.left.max(usize::try_from(dx).unwrap_or(0));
            let last = ink
                .right
                .min(usize::try_from(width as isize + dx.min(0)).unwrap_or(0));
            if first >= last {
                continue;
            }
            let source = &coverage[source_y * width + first..source_y * width + last];
            let start = y * width + (first as isize - dx) as usize;
            let out = &mut band[start..start + source.len()];
            for (pixel, value) in out.iter_mut().zip(source) {
                *pixel = (*pixel).max((u32::from(*value) * weight / 255) as u8);
            }
        }
    }
    band
}

/// A rectangle of pixels, right and bottom exclusive.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct Bounds {
    left: usize,
    top: usize,
    right: usize,
    bottom: usize,
}

fn ink_bounds(coverage: &[u8], width: usize, height: usize) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;
    for y in 0..height {
        let row = &coverage[y * width..(y + 1) * width];
        let Some(left) = row.iter().position(|&value| value > 0) else {
            continue;
        };
        let right = row.iter().rposition(|&value| value > 0).unwrap_or(left) + 1;
        bounds = Some(bounds.map_or(
            Bounds {
                left,
                top: y,
                right,
                bottom: y + 1,
            },
            |bounds| Bounds {
                left: bounds.left.min(left),
                top: bounds.top,
                right: bounds.right.max(right),
                bottom: y + 1,
            },
        ));
    }
    bounds
}

/// Disc taps split into full-weight row runs and partially weighted rim taps.
struct Disc {
    extent: usize,
    /// Run half-width and the row offsets whose full taps are exactly `-half..=half`.
    runs: Vec<(usize, Vec<isize>)>,
    /// `(dx, dy, weight)` for taps weighted strictly between 0 and 255.
    rim: Vec<(isize, isize, u32)>,
}

impl Disc {
    /// A tap's weight is its area within `radius + 0.5` of the center, in 255ths.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        clippy::cast_possible_wrap
    )]
    fn new(radius: f32) -> Self {
        let extent = disc_extent(radius) as isize;
        let mut runs: Vec<(usize, Vec<isize>)> = Vec::new();
        let mut rim = Vec::new();
        for dy in -extent..=extent {
            let mut half = None;
            for dx in -extent..=extent {
                let distance = ((dx * dx + dy * dy) as f32).sqrt();
                let weight = ((radius + 0.5 - distance).clamp(0.0, 1.0) * 255.0).round() as u32;
                match weight {
                    0 => {}
                    // Weight falls with distance, so a row's full taps form one centered run.
                    255 => half = Some(dx.unsigned_abs()),
                    _ => rim.push((dx, dy, weight)),
                }
            }
            if let Some(half) = half {
                match runs.iter_mut().find(|(run, _)| *run == half) {
                    Some((_, offsets)) => offsets.push(dy),
                    None => runs.push((half, vec![dy])),
                }
            }
        }
        Self {
            extent: extent as usize,
            runs,
            rim,
        }
    }
}

/// Scratch for van Herk/Gil-Werman sliding-window maxima.
#[derive(Default)]
struct WindowMax {
    padded: Vec<u8>,
    from_start: Vec<u8>,
    to_end: Vec<u8>,
}

impl WindowMax {
    /// `out[i]` = max of `row` over `left + i - half ..= left + i + half`, 0 outside the row.
    fn of(&mut self, row: &[u8], left: usize, count: usize, half: usize, out: &mut [u8]) {
        let window = 2 * half + 1;
        let length = count + 2 * half;
        self.padded.clear();
        self.padded.resize(length, 0);
        // Padded index i is column left - half + i.
        let first_column = left.saturating_sub(half);
        let last_column = (left + count + half).min(row.len());
        if first_column < last_column {
            let at = first_column + half - left;
            self.padded[at..at + last_column - first_column]
                .copy_from_slice(&row[first_column..last_column]);
        }
        self.from_start.clear();
        self.from_start.resize(length, 0);
        self.to_end.clear();
        self.to_end.resize(length, 0);
        for block in (0..length).step_by(window) {
            let end = (block + window).min(length);
            let mut highest = 0;
            for index in block..end {
                highest = highest.max(self.padded[index]);
                self.from_start[index] = highest;
            }
            highest = 0;
            for index in (block..end).rev() {
                highest = highest.max(self.padded[index]);
                self.to_end[index] = highest;
            }
        }
        for (index, value) in out[..count].iter_mut().enumerate() {
            *value = self.to_end[index].max(self.from_start[index + window - 1]);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "clamp_tests.rs"]
mod clamp_tests;

#[cfg(test)]
#[path = "runs_tests.rs"]
mod runs_tests;

#[cfg(test)]
#[path = "box_tests.rs"]
mod box_tests;
