//! Text shaping and rasterization for alpha2 hosts.
//!
//! The engine takes text as a host texture ([`TextTexture`]); this crate is
//! one host for it, in pure Rust, from font bytes the caller supplies. Each
//! paragraph is split by direction (UAX #9), script, and font, rustybuzz
//! shapes each run, lines break where UAX #14 allows, and each glyph's
//! outline is drawn by `ab_glyph_rasterizer` when it is placed, or its
//! color form (a COLR painting or a CBDT or sbix bitmap) by `color`. It builds
//! the same way natively and for wasm, and the same request with the same
//! fonts gives the same pixels on every target, so a browser preview and a
//! native export of one project draw identical text. `alpha2/text` exposes
//! it to JavaScript as `WasmTextRasterizer`.

use std::cell::OnceCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
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

mod color;
mod unicode_tables;

const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_FONT_SIZE: f32 = 2_048.0;
const MAX_LINE_HEIGHT: f32 = 10.0;
const MAX_OUTLINE_WIDTH: f32 = 64.0;
const MAX_BACKGROUND_PADDING: f32 = 256.0;
/// The letter spacing a request takes, in font sizes.
const LETTER_SPACING_RANGE: std::ops::RangeInclusive<f32> = -0.5..=1.0;

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
    /// Baseline-to-baseline distance in font sizes. Each line's em box,
    /// 0.8 of a font size above its baseline and 0.2 below, is centred in
    /// its line when the line is taller than an em.
    pub line_height: f32,
    /// Per-line horizontal alignment.
    pub align: TextAlign,
    /// Straight (not premultiplied), sRGB-encoded RGBA color. The output is
    /// premultiplied by glyph coverage and this alpha. A color glyph keeps
    /// its own colors, but for the parts its font draws in the text's
    /// color, and takes this alpha as its opacity.
    pub color: [u8; 4],
    /// Whether color glyphs are drawn in color, and which fonts emoji take.
    pub emoji_policy: EmojiPolicy,
    /// Caller-managed version. Change it whenever content/style/font inputs change.
    pub content_version: u64,
    /// A band around the glyphs, drawn behind them.
    pub outline: Option<TextOutline>,
    /// A box behind each line, drawn behind the outline.
    pub background: Option<TextBackground>,
    /// The family drawn first, as [`font_family`] reads it, matched without
    /// regard to ASCII case; the rest of the stack fills in what it lacks.
    /// `None`, or a family no font has, keeps the fallback order.
    pub font_family: Option<String>,
    /// Space added after each typographic character unit (grapheme
    /// cluster), in pixels, as CSS `letter-spacing` adds it: none after a
    /// line's last, and none after a character of a cursive script (Arabic,
    /// say), whose joins it would break. Not zero, it turns optional
    /// ligatures off. In `[-0.5, 1]` font sizes.
    pub letter_spacing: f32,
    /// The weight, in `[1, 1000]`, a family's faces are matched by (CSS
    /// Fonts 4 weight matching), and a variable face's `wght` is set to,
    /// clamped to its axis. No face is emboldened: a weight a family lacks
    /// draws its nearest face.
    pub font_weight: u16,
}

/// A band of `width` pixels around every glyph, behind it, in a straight
/// (not premultiplied), sRGB-encoded RGBA color.
///
/// The band is the glyph coverage dilated by a disc: each pixel takes the
/// highest coverage within `width` of it, weighted down over the disc's last
/// pixel so the band's edge is antialiased.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextOutline {
    /// Straight, sRGB-encoded RGBA.
    pub color: [u8; 4],
    /// Radius of the band in pixels, in `(0, 64]`.
    pub width: f32,
}

/// A box behind each non-empty line: the line's em box, as wide as the
/// line's advance and one font size tall, grown by `padding` pixels on
/// every side. Boxes that meet merge.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct TextBackground {
    /// Straight, sRGB-encoded RGBA.
    pub color: [u8; 4],
    /// Space between the text and the box edge, in `[0, 256]` pixels.
    pub padding: f32,
}

/// Rasterized pixels and their text-specific layout descriptor.
#[derive(Clone, Debug, PartialEq)]
pub struct TextRasterResult {
    /// Layout facts sharing the generic compositor texture contract.
    pub descriptor: TextTexture,
    /// Tightly packed premultiplied RGBA8 pixels.
    pub pixels: HostTexturePixels,
}

/// Where a request's text lies, without drawing it
/// ([`TextRasterizer::measure`]): rectangles as `[left, top, right, bottom]`
/// in pixels from the layout box's top-left corner, y down. The texture
/// [`TextRasterizer::rasterize`] draws for the request holds that corner at
/// ([`TextTexture::layout_x`], [`TextTexture::layout_y`]).
#[derive(Clone, Debug, PartialEq)]
pub struct TextMeasure {
    /// The layout box's width in whole pixels: the width the request wraps
    /// at, else its widest line, rounded up.
    pub box_width: u32,
    /// The layout box's height in whole pixels: every line's em box, rounded
    /// up.
    pub box_height: u32,
    /// [`TextRasterRequest::decoration_margin`].
    pub decoration_margin: u32,
    /// From the start of the line that starts farthest left to the end of
    /// the one that ends farthest right, the box's height, grown by the
    /// decoration margin as the box is. It moves only as the lines' advances
    /// do, not as glyphs with more or less ink are typed.
    pub content: [f32; 4],
    /// What the text draws: each glyph's outline (its curves, not their
    /// control points) or color form, the outline's band around them, and
    /// the background's boxes. Every pixel the raster gives any alpha meets
    /// it, and it reaches at most a pixel past them, or past a COLR glyph's
    /// outlines and clip boxes, which may hold a little more than it paints.
    /// `None` when the text draws nothing.
    pub ink: Option<[f32; 4]>,
    /// Number of laid-out lines.
    pub line_count: u32,
    /// The first line's alphabetic baseline, in pixels from the layout box's
    /// top.
    pub first_baseline: f32,
    /// The last line's alphabetic baseline, in pixels from the layout box's
    /// top.
    pub last_baseline: f32,
}

/// A request's text shaped once, for layout: its lines at any wrapping
/// width, broken as [`TextRasterizer::rasterize`] breaks them, without
/// shaping again ([`TextRasterizer::shape`]).
#[derive(Clone, Debug)]
pub struct ShapedText {
    /// Each paragraph's clusters, in logical order.
    paragraphs: Vec<Vec<Cluster>>,
    font_size: f32,
    line_height: f32,
    line_advance: f32,
}

/// Lines of a shaped text at one wrapping width ([`ShapedText::lines_at`]).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct LineMetrics {
    /// Number of lines, as [`TextMeasure::line_count`] counts them.
    pub line_count: u32,
    /// The widest line's advance, trailing spaces left out.
    pub widest: f32,
    /// Every line's em box, unrounded: the rasterizer's box height before it
    /// is rounded up.
    pub height: f32,
    /// The first line's alphabetic baseline, in pixels from the box's top, as
    /// [`TextMeasure::first_baseline`].
    pub first_baseline: f32,
    /// The last line's alphabetic baseline, in pixels from the box's top.
    pub last_baseline: f32,
}

impl ShapedText {
    /// The widest run between line-break opportunities, trailing spaces
    /// left out (CSS min-content).
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

    /// The widest paragraph unwrapped, trailing spaces left out (CSS
    /// max-content).
    #[must_use]
    pub fn max_content_width(&self) -> f32 {
        self.paragraphs
            .iter()
            .map(|clusters| advance(clusters))
            .fold(0.0, f32::max)
    }

    /// The lines at `max_width`, as [`TextRasterizer::rasterize`] breaks
    /// them; a non-positive width does not wrap.
    #[must_use]
    pub fn lines_at(&self, max_width: f32) -> LineMetrics {
        let (mut count, mut kept, mut widest) = (0_u32, 1_u32, 0.0_f32);
        for clusters in &self.paragraphs {
            for range in break_lines(clusters, max_width) {
                count = count.saturating_add(1);
                if !range.is_empty() {
                    kept = count;
                }
                widest = widest.max(advance(&clusters[range]));
            }
        }
        let [first_baseline, last_baseline] =
            baselines(kept, self.font_size, self.line_height, self.line_advance);
        LineMetrics {
            line_count: kept,
            widest,
            height: box_height(kept, self.line_advance, self.font_size),
            first_baseline,
            last_baseline,
        }
    }
}

/// The pen advance over `clusters` in logical order, as [`break_lines`]
/// sums it, up to the last that does not hang, without the last's letter
/// spacing: a line ends at its last character, not past its spacing.
fn advance(clusters: &[Cluster]) -> f32 {
    let Some(last) = clusters.iter().rposition(|cluster| !cluster.hangs) else {
        return 0.0;
    };
    clusters[..=last]
        .iter()
        .fold(0.0, |width, cluster| width + cluster.width)
        - clusters[last].spacing
}

/// Space above a line's em box in its line: half the leading when lines are
/// farther apart than an em (CSS half-leading), else none, so the box still
/// holds every em box.
#[must_use]
pub fn half_leading(font_size: f32, line_height: f32) -> f32 {
    (font_size * line_height - font_size).max(0.0) * 0.5
}

/// The first and last of `line_count` lines' baselines, from the box's top:
/// each line's em box top plus its nominal ascent, 0.8 of a font size, summed
/// as [`Layout`] places them.
#[allow(clippy::cast_precision_loss)]
fn baselines(line_count: u32, font_size: f32, line_height: f32, line_advance: f32) -> [f32; 2] {
    let (leading, ascent) = (half_leading(font_size, line_height), font_size * 0.8);
    let last = line_count.saturating_sub(1) as f32;
    [leading + ascent, last * line_advance + leading + ascent]
}

/// Every line's nominal em box: the last one's too when lines are closer
/// than an em.
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
    /// The family its name table gives, if any.
    family: Option<String>,
    /// OS/2 usWeightClass in [1, 1000]; 400 without the table.
    weight: u16,
    /// The fvar `wght` axis (min, default, max), for a variable face.
    wght: Option<(f32, f32, f32)>,
}

const WGHT: ttf_parser::Tag = ttf_parser::Tag::from_bytes(b"wght");

impl LoadedFont {
    /// Checks that the bytes hold the face and reads its family and weight.
    /// Nothing else is read until a request draws with it: a glyph's outline
    /// when the glyph is placed.
    fn parse(source: TextFont) -> Result<Self, TextRasterError> {
        let face = ttf_parser::Face::parse(&source.bytes, source.face_index)
            .map_err(|error| TextRasterError::InvalidFont(error.to_string()))?;
        let weight = face.weight().to_number().clamp(1, 1000);
        let wght = face
            .variation_axes()
            .into_iter()
            .find(|axis| axis.tag == WGHT)
            .map(|axis| (axis.min_value, axis.def_value, axis.max_value));
        let family = font_family(&source.bytes, source.face_index);
        Ok(Self {
            source,
            family,
            weight,
            wght,
        })
    }

    /// The weights it draws, `[min, max]`: its axis's, rounded inward to
    /// whole weights, for a variable face; its weight for a static one.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn weights(&self) -> (u16, u16) {
        self.wght
            .map_or((self.weight, self.weight), |(min, _, max)| {
                let whole = |value: f32| value.clamp(1.0, 1000.0) as u16;
                (whole(min.ceil()), whole(max.floor()).max(whole(min.ceil())))
            })
    }
}

/// The faces of one family in the order a request for `desired` tries them,
/// as CSS Fonts 4 (section 5.2) matches weights, ties in the order given.
/// `weights` holds each face's range, `[w, w]` for a static face: one that
/// holds `desired` matches exactly; otherwise its nearest end is its weight.
/// From 400 to 500 the faces up to 500 come first, nearest first, then those
/// below, nearest first, then those above 500; below 400 those below, then
/// those above; above 500 those above, then those below.
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

/// Checks that `bytes` hold an OpenType face at `face_index`, as adding the
/// font to a rasterizer checks it, without keeping anything: a host can
/// refuse a file at import that the rasterizer would refuse later.
///
/// # Errors
/// Returns [`TextRasterError::InvalidFont`] with the parser's reason (a WOFF
/// or WOFF2 container's unknown magic, say).
pub fn check_font(bytes: &[u8], face_index: u32) -> Result<(), TextRasterError> {
    ttf_parser::Face::parse(bytes, face_index)
        .map(|_| ())
        .map_err(|error| TextRasterError::InvalidFont(error.to_string()))
}

/// How many faces `bytes` hold: a font collection's (`.ttc`, `.otc`) count,
/// else 1. A host registers each of them to draw every weight a collection
/// holds.
#[must_use]
pub fn face_count(bytes: &[u8]) -> u32 {
    ttf_parser::fonts_in_collection(bytes).unwrap_or(1)
}

/// The family a font's name table gives the face at `face_index`: its
/// typographic family (name ID 16), else its family (name ID 1), in US
/// English where the table has it, else in the first entry that decodes.
/// `None` for bytes that do not parse or name no family.
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

/// The pixel bytes a [`TextRasterizer`] keeps cached by default: room for a
/// few full-frame 4K titles, or hundreds of caption-sized ones.
pub const DEFAULT_TEXT_CACHE_BYTES: usize = 64 << 20;

/// What a cached result costs beyond its pixels, so that results with few or
/// no pixels still count toward the budget and their number stays bounded.
const CACHE_ENTRY_OVERHEAD: usize = 256;

/// Deterministic text host with version-key result caching.
///
/// Results are cached by content version, least recently used first out,
/// within a byte budget ([`DEFAULT_TEXT_CACHE_BYTES`] unless
/// [`Self::set_cache_budget`] changes it). A cached result is shared, not
/// copied: a request whose version is cached returns the same [`Arc`].
#[derive(Debug)]
pub struct TextRasterizer {
    fonts: Vec<LoadedFont>,
    cache: ResultCache,
}

/// Rasterized results by content version, within a byte budget, evicting the
/// least recently used.
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

    /// Keeps `result` under `version` if it fits the budget, evicting the
    /// least recently used results to make room.
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
        })
    }

    /// Appends a font to the end of the fallback stack.
    ///
    /// Fonts change what every request draws, so the result cache is
    /// cleared.
    ///
    /// # Errors
    /// Returns [`TextRasterError::InvalidFont`] for malformed font bytes.
    pub fn add_font(&mut self, source: TextFont) -> Result<(), TextRasterError> {
        self.fonts.push(LoadedFont::parse(source)?);
        self.cache.clear();
        Ok(())
    }

    /// Number of fonts in the fallback stack.
    #[must_use]
    pub fn font_count(&self) -> usize {
        self.fonts.len()
    }

    /// Shapes, lays out, and rasterizes one request, or returns the cached
    /// result of its exact version, shared rather than copied.
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
        let result = Arc::new(self.rasterize_uncached(request)?);
        self.cache.insert(request.content_version, &result);
        Ok(result)
    }

    /// Drops a single host version after content or font invalidation.
    pub fn invalidate(&mut self, content_version: u64) {
        self.cache.remove(content_version);
    }

    /// Sets how many bytes of results the cache keeps, evicting the least
    /// recently used down to it. Each result counts its pixels and a small
    /// fixed overhead; a result larger than the budget is returned but not
    /// kept. Zero keeps nothing.
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
    }

    /// Number of versioned raster results currently retained.
    #[must_use]
    pub fn cache_len(&self) -> usize {
        self.cache.entries.len()
    }

    /// Where `request`'s text lies, laid out as [`Self::rasterize`] lays it
    /// out, without drawing it or touching the result cache.
    ///
    /// # Errors
    /// Returns the validation, font, or size error [`Self::rasterize`]
    /// returns for the same request.
    #[allow(clippy::cast_precision_loss)]
    pub fn measure(&self, request: &TextRasterRequest) -> Result<TextMeasure, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let faces = Faces::new(&self.fonts, request.font_weight)?;
        let layout = self.lay_out(&faces, request)?;
        let mut ink = None;
        layout.each_glyph(
            &faces,
            request,
            &mut HashMap::new(),
            |drawing, origin, scale, _| {
                if let Some([x_min, y_min, x_max, y_max]) = drawing.ink_bounds() {
                    include(
                        &mut ink,
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
        // The band reaches past each pixel with ink by the disc's radius and one more pixel for
        // the rim taps it weights in part: see `Disc::new`.
        let band = request.outline.map_or(0.0, |outline| outline.width + 1.0);
        ink = ink.map(|bounds| grown(bounds, band));
        let padding = request.background.map(|background| background.padding);
        let mut content = None;
        for (index, line) in layout.lines.iter().enumerate() {
            let left = layout.line_offset(line, request.align);
            let top = layout.line_top(index);
            include(&mut content, [left, 0.0, left + line.width, layout.height]);
            if let (Some(padding), true) = (padding, line.width > 0.0) {
                include(
                    &mut ink,
                    grown(
                        [left, top, left + line.width, top + request.font_size],
                        padding,
                    ),
                );
            }
        }
        let content = content.unwrap_or([0.0, 0.0, 0.0, layout.height]);
        let last = layout.lines.len().saturating_sub(1);
        Ok(TextMeasure {
            first_baseline: layout.line_top(0) + layout.ascent,
            last_baseline: layout.line_top(last) + layout.ascent,
            box_width: layout.box_width,
            box_height: layout.box_height,
            decoration_margin: request.decoration_margin(),
            content: grown(content, request.decoration_margin() as f32),
            ink,
            line_count: layout.line_count,
        })
    }

    /// Shapes `request`'s text as [`Self::rasterize`] would, for layout: its
    /// lines at any wrapping width come from the result without shaping
    /// again. The request's `max_width`, color, outline, background and
    /// content version are ignored.
    ///
    /// # Errors
    /// Returns [`TextRasterError::NoFonts`] without fonts, and
    /// [`TextRasterError::InvalidRequest`] for a size, line height or text
    /// [`Self::rasterize`] rejects.
    pub fn shape(&self, request: &TextRasterRequest) -> Result<ShapedText, TextRasterError> {
        validate_request(&TextRasterRequest {
            text: String::new(),
            max_width: 0.0,
            content_version: 1,
            outline: None,
            background: None,
            ..request.clone()
        })?;
        if request.text.len() > MAX_TEXT_BYTES {
            return Err(TextRasterError::InvalidRequest);
        }
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let faces = Faces::new(&self.fonts, request.font_weight)?;
        let order = self.font_order(request.font_family.as_deref(), request.font_weight);
        let paragraphs = normalize(&request.text, request.emoji_policy)
            .split('\n')
            .map(|paragraph| {
                let bidi = ParagraphBidiInfo::new(paragraph, None);
                let glyphs = shape_paragraph(
                    &faces,
                    paragraph,
                    &bidi,
                    request.font_size,
                    &order,
                    request.emoji_policy,
                    request.letter_spacing,
                );
                clusters(&glyphs, paragraph, request.letter_spacing)
            })
            .collect();
        Ok(ShapedText {
            paragraphs,
            font_size: request.font_size,
            line_height: request.line_height,
            line_advance: request.font_size * request.line_height,
        })
    }

    /// `request`'s lines, broken and placed in its layout box.
    #[allow(clippy::cast_precision_loss)]
    fn lay_out(
        &self,
        faces: &Faces<'_>,
        request: &TextRasterRequest,
    ) -> Result<Layout, TextRasterError> {
        let line_advance = request.font_size * request.line_height;
        let order = self.font_order(request.font_family.as_deref(), request.font_weight);
        let mut lines = Vec::new();
        // Lines up to the last with glyphs stay, however little ink they have; blank lines after it
        // take no room.
        let mut kept = 1;
        for paragraph in normalize(&request.text, request.emoji_policy).split('\n') {
            for line in layout_paragraph(
                faces,
                paragraph,
                request.font_size,
                request.max_width,
                &order,
                request.emoji_policy,
                request.letter_spacing,
            ) {
                lines.push(line);
                if lines.last().is_some_and(|line| line.has_glyphs) {
                    kept = lines.len();
                }
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
        let height = box_height(line_count, line_advance, request.font_size).max(1.0);
        Ok(Layout {
            box_width: checked_dimension(width)?,
            box_height: checked_dimension(height)?,
            lines,
            line_count,
            line_advance,
            half_leading: half_leading(request.font_size, request.line_height),
            ascent: request.font_size * 0.8,
            descent: request.font_size * 0.2,
            width,
            height,
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
        let faces = Faces::new(&self.fonts, request.font_weight)?;
        let layout = self.lay_out(&faces, request)?;
        let (box_width, box_height) = (layout.box_width, layout.box_height);
        let mut drawings = HashMap::<(usize, u16), Drawing>::new();
        let mut placed = Vec::new();
        // Ink past the box on each axis, and the ink's own reach above and below its baselines.
        let (mut overflow_x, mut overflow_y) = (0_i64, 0_i64);
        let (mut ink_ascent, mut ink_descent) = (layout.ascent, layout.descent);
        layout.each_glyph(
            &faces,
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
                placed.push(((glyph.font, glyph.glyph_index), origin, scale, pixels));
            },
        );
        // The texture is the box plus a margin, equal on both ends of an axis so the box stays
        // centred, with room for decorations and for outlined ink past the box.
        let (outline, padding) = decoration_extents(request);
        let margin = |overflow: i64| -> Result<u32, TextRasterError> {
            u32::try_from(overflow)
                .map_err(|_| TextRasterError::TooLarge)?
                .checked_add(outline)
                .map(|margin| margin.max(outline.max(padding)))
                .ok_or(TextRasterError::TooLarge)
        };
        let (margin_x, margin_y) = (margin(overflow_x)?, margin(overflow_y)?);
        let width = margin_x
            .checked_mul(2)
            .and_then(|margins| box_width.checked_add(margins))
            .ok_or(TextRasterError::TooLarge)?;
        let height = margin_y
            .checked_mul(2)
            .and_then(|margins| box_height.checked_add(margins))
            .ok_or(TextRasterError::TooLarge)?;
        if width > 16_384 || height > 16_384 {
            return Err(TextRasterError::TooLarge);
        }
        let pixel_count = usize::try_from(width)
            .ok()
            .and_then(|width| {
                usize::try_from(height)
                    .ok()
                    .and_then(|height| width.checked_mul(height))
            })
            .ok_or(TextRasterError::TooLarge)?;
        let mut coverage = vec![0_u8; pixel_count];
        // Color glyphs, premultiplied RGBA, only once one is placed.
        let mut colors: Option<Vec<u8>> = None;
        let mut line_boxes = Vec::with_capacity(layout.lines.len());
        for (index, line) in layout.lines.iter().enumerate() {
            if line.width > 0.0 {
                line_boxes.push((
                    layout.line_offset(line, request.align),
                    line.width,
                    layout.line_top(index),
                ));
            }
        }
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
        let rgba8 = compose(
            request,
            &coverage,
            colors.as_deref(),
            width,
            [margin_x, margin_y],
            &line_boxes,
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
                first_baseline: layout.half_leading + layout.ascent,
                ascent: ink_ascent,
                descent: ink_descent,
                line_count: layout.line_count,
            },
            pixels: HostTexturePixels { texture, rgba8 },
        })
    }

    /// Font indices in the order a request tries them: families in
    /// fallback order (the order their first faces were registered),
    /// `family` first; inside a family, faces by CSS Fonts 4's weight
    /// matching for `weight` ([`weight_order`]), ties in registration order.
    fn font_order(&self, family: Option<&str>, weight: u16) -> Vec<usize> {
        // Faces grouped by family, matched without regard to ASCII case; a face without one is a
        // family of its own.
        let mut groups: Vec<(Option<String>, Vec<usize>)> = Vec::new();
        for (index, font) in self.fonts.iter().enumerate() {
            let name = font.family.as_deref().map(str::to_ascii_lowercase);
            match groups
                .iter_mut()
                .find(|(held, _)| name.is_some() && *held == name)
            {
                Some((_, faces)) => faces.push(index),
                None => groups.push((name, vec![index])),
            }
        }
        let wanted = family.map(str::to_ascii_lowercase);
        if let Some(at) = groups
            .iter()
            .position(|(name, _)| wanted.is_some() && *name == wanted)
        {
            let preferred = groups.remove(at);
            groups.insert(0, preferred);
        }
        groups
            .into_iter()
            .flat_map(|(_, faces)| {
                let weights = faces
                    .iter()
                    .map(|&index| self.fonts[index].weights())
                    .collect::<Vec<_>>();
                weight_order(&weights, weight)
                    .into_iter()
                    .map(move |at| faces[at])
            })
            .collect()
    }
}

/// The stack's faces for one request, each parsed once; a face's shaping
/// tables only when a run shapes with it.
struct Faces<'a> {
    parsed: Vec<ttf_parser::Face<'a>>,
    shapers: Vec<OnceCell<rustybuzz::Face<'a>>>,
    /// Whether each face has color glyphs (a COLR, CBDT, or sbix table).
    color: Vec<bool>,
}

impl<'a> Faces<'a> {
    /// The faces at `weight`: a variable face at that `wght`, clamped to its
    /// axis, so one request draws one instance of each face.
    fn new(fonts: &'a [LoadedFont], weight: u16) -> Result<Self, TextRasterError> {
        let parsed = fonts
            .iter()
            .map(|font| {
                let mut face = ttf_parser::Face::parse(&font.source.bytes, font.source.face_index)
                    .ok()
                    .filter(|face| face.units_per_em() > 0)
                    .ok_or_else(|| TextRasterError::InvalidFont(font.source.family.clone()))?;
                if let Some((min, _, max)) = font.wght {
                    face.set_variation(WGHT, f32::from(weight).clamp(min, max));
                }
                Ok(face)
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            shapers: parsed.iter().map(|_| OnceCell::new()).collect(),
            color: parsed
                .iter()
                .map(|face| {
                    let tables = face.tables();
                    tables.colr.is_some() || tables.cbdt.is_some() || tables.sbix.is_some()
                })
                .collect(),
            parsed,
        })
    }

    /// The face's shaper, at the variation the parsed face is set to.
    fn shaper(&self, font: usize) -> &rustybuzz::Face<'a> {
        self.shapers[font].get_or_init(|| rustybuzz::Face::from_face(self.parsed[font].clone()))
    }

    #[allow(clippy::cast_precision_loss)]
    fn units_per_em(&self, font: usize) -> f32 {
        f32::from(self.parsed[font].units_per_em())
    }

    /// Whether the font maps `character` to a glyph of its own.
    fn has(&self, font: usize, character: char) -> bool {
        self.parsed[font]
            .glyph_index(character)
            .is_some_and(|glyph| glyph.0 != 0)
    }

    /// Whether the font has every character of `grapheme` that is drawn at all.
    fn covers(&self, font: usize, grapheme: &str) -> bool {
        grapheme
            .chars()
            .filter(|&character| !is_default_ignorable(character))
            .all(|character| self.has(font, character))
    }
}

/// The text as layout reads it: under [`EmojiPolicy::Replace`] each emoji
/// grapheme one replacement character, every line or paragraph separator a
/// `\n`, and a tab a space (a caption has no tab stops).
///
/// U+2028 LINE SEPARATOR, which UAX #9 does not count as a paragraph end,
/// ends one here too, so every line break is one path.
fn normalize(text: &str, policy: EmojiPolicy) -> String {
    let mut normalized = String::with_capacity(text.len());
    for grapheme in text.graphemes(true) {
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
    }
    normalized
}

/// A grapheme shown as an emoji (UTS #51), as `OffscreenTextRasterizer`
/// reads one too: unless U+FE0E asks for text, one holding an
/// `Emoji_Presentation` character (regional indicators and skin tones
/// included), or U+FE0F with a pictograph or a keycap. A symbol shown as
/// text by default, such as (c) or a sun, is not one: a text font draws it.
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

/// Whether `text` is of a cursive script, which takes no letter spacing
/// (CSS Text 3's list: Adlam, Arabic, Hanifi Rohingya, Mandaic, Mongolian,
/// N'Ko, Phags-pa, Syriac), by its first character of a script of its own.
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

/// A space a line may end in: it takes no width there, hanging past the
/// end. Not the no-break spaces, which hold their neighbours together, nor
/// the Ogham space mark, which is drawn.
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

/// A request's lines, broken and placed: pixels from its layout box's
/// top-left corner, y down.
struct Layout {
    lines: Vec<LaidLine>,
    line_count: u32,
    /// Baseline to baseline.
    line_advance: f32,
    /// Space above each line's em box in its line ([`half_leading`]).
    half_leading: f32,
    /// The nominal ascent and descent lines are spaced by.
    ascent: f32,
    descent: f32,
    /// The layout box: as wide as the request wraps at, else its widest
    /// line, and every line's em box tall.
    width: f32,
    height: f32,
    /// The box rounded up to whole pixels, as the texture holds it.
    box_width: u32,
    box_height: u32,
}

impl Layout {
    /// Where `line` starts in the box: Start and End are the paragraph's own sides.
    fn line_offset(&self, line: &LaidLine, align: TextAlign) -> f32 {
        match (align, line.rtl) {
            (TextAlign::Center, _) => (self.width - line.width) * 0.5,
            (TextAlign::Start, false) | (TextAlign::End, true) => 0.0,
            (TextAlign::End, false) | (TextAlign::Start, true) => self.width - line.width,
        }
    }

    /// The top of line `index`'s em box, from the box's top.
    #[allow(clippy::cast_precision_loss)]
    fn line_top(&self, index: usize) -> f32 {
        index as f32 * self.line_advance + self.half_leading
    }

    /// Calls `place` with each glyph's drawing, read once a glyph into
    /// `drawings`, its origin in the box, and its pixels a font unit.
    #[allow(clippy::cast_precision_loss)]
    fn each_glyph(
        &self,
        faces: &Faces<'_>,
        request: &TextRasterRequest,
        drawings: &mut HashMap<(usize, u16), Drawing>,
        mut place: impl FnMut(&Drawing, [f32; 2], f32, &PositionedGlyph),
    ) {
        for (index, line) in self.lines.iter().enumerate() {
            let offset = self.line_offset(line, request.align);
            let baseline = self.line_top(index) + self.ascent;
            for glyph in &line.glyphs {
                let drawing = drawings
                    .entry((glyph.font, glyph.glyph_index))
                    .or_insert_with(|| Drawing::of(faces, glyph.font, glyph.glyph_index, request));
                let scale = request.font_size / faces.units_per_em(glyph.font);
                let origin = [glyph.x + glyph.x_offset + offset, baseline - glyph.y_offset];
                place(drawing, origin, scale, glyph);
            }
        }
    }
}

/// Grows `bounds` to hold `[left, top, right, bottom]`.
fn include(bounds: &mut Option<[f32; 4]>, [left, top, right, bottom]: [f32; 4]) {
    let held = bounds.get_or_insert([left, top, right, bottom]);
    *held = [
        held[0].min(left),
        held[1].min(top),
        held[2].max(right),
        held[3].max(bottom),
    ];
}

/// `[left, top, right, bottom]` grown by `by` on every side.
fn grown([left, top, right, bottom]: [f32; 4], by: f32) -> [f32; 4] {
    [left - by, top - by, right + by, bottom + by]
}

/// One line of a paragraph, laid out.
struct LaidLine {
    /// Glyphs left to right, pens from the line's start.
    glyphs: Vec<PositionedGlyph>,
    /// Width of what it draws: its trailing spaces hang past it.
    width: f32,
    /// The paragraph runs right to left.
    rtl: bool,
    /// It has glyphs, even if none with ink.
    has_glyphs: bool,
}

/// Lays out one paragraph: shaped run by run in logical order, broken into
/// lines no wider than `max_width` (none when it is not positive), and each
/// line put in visual order.
#[allow(clippy::too_many_arguments)]
fn layout_paragraph(
    faces: &Faces<'_>,
    paragraph: &str,
    font_size: f32,
    max_width: f32,
    order: &[usize],
    policy: EmojiPolicy,
    letter_spacing: f32,
) -> Vec<LaidLine> {
    let bidi = ParagraphBidiInfo::new(paragraph, None);
    let rtl = bidi.paragraph_level.is_rtl();
    let glyphs = shape_paragraph(
        faces,
        paragraph,
        &bidi,
        font_size,
        order,
        policy,
        letter_spacing,
    );
    let clusters = clusters(&glyphs, paragraph, letter_spacing);
    break_lines(&clusters, max_width)
        .into_iter()
        .map(|range| {
            let has_glyphs = !range.is_empty();
            // Trailing spaces hang past the end: no width, nothing drawn.
            let mut end = range.end;
            while end > range.start && clusters[end - 1].hangs {
                end -= 1;
            }
            let mut line = LaidLine {
                glyphs: Vec::new(),
                width: 0.0,
                rtl,
                has_glyphs,
            };
            if end == range.start {
                return line;
            }
            let bytes = clusters[range.start].start
                ..clusters
                    .get(range.end)
                    .map_or(paragraph.len(), |next| next.start);
            let levels = bidi.reordered_levels(bytes);
            // Each glyph's cluster, for the spacing a cluster takes after it in visual order: a gap
            // between neighbours, so none after the line's last on either side.
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
                    x,
                    x_offset: glyph.x_offset,
                    y_offset: glyph.y_offset,
                });
                x += glyph.advance;
            }
            line.width = x;
            line
        })
        .collect()
}

/// The glyphs of `range`, in logical order, in visual order (UAX #9 L2):
/// from the highest level down to the lowest odd one, every run at that
/// level or above reversed. A right-to-left run comes back to the order its
/// shaper gave.
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

/// One stretch of a paragraph shaped as a unit: one direction, one
/// script, one font.
#[derive(Clone, Debug, PartialEq)]
struct Run {
    range: Range<usize>,
    font: usize,
    script: Option<Script>,
    rtl: bool,
}

/// Splits a paragraph into runs. Each grapheme takes the first font in
/// `order` with all it draws; one of only common characters (spaces, digits,
/// punctuation) stays in the font before it when that has them, and one no
/// font has takes the font of its base character, else the font before it.
/// A grapheme shown as an emoji tries the color fonts first under
/// [`EmojiPolicy::Color`] and the others first otherwise, as one asking for
/// text (U+FE0E) does, and stays only in a font of the kind it tries first
/// when the stack has one.
/// A grapheme of common characters takes the script before it, or after it
/// at the paragraph's start.
fn itemize(
    faces: &Faces<'_>,
    paragraph: &str,
    bidi: &ParagraphBidiInfo<'_>,
    order: &[usize],
    policy: EmojiPolicy,
) -> Vec<Run> {
    let first = order.first().copied().unwrap_or(0);
    let (color, text): (Vec<usize>, Vec<usize>) =
        order.iter().partition(|&&font| faces.color[font]);
    let color_first = color.iter().chain(&text).copied().collect::<Vec<_>>();
    let text_first = text.iter().chain(&color).copied().collect::<Vec<_>>();
    // Whether a font is of the kind a grapheme wants, or the stack has none of that kind.
    let fits = |font: usize, color_wanted: bool| {
        faces.color[font] == color_wanted
            || if color_wanted {
                color.is_empty()
            } else {
                text.is_empty()
            }
    };
    let mut graphemes = Vec::<(usize, usize, Option<Script>, bool)>::new();
    let mut current = None;
    for (start, grapheme) in paragraph.grapheme_indices(true) {
        let script = grapheme
            .chars()
            .map(|character| character.script())
            .find(|script| !matches!(script, Script::Common | Script::Inherited | Script::Unknown));
        // Whether the grapheme wants a color font, when it wants one kind.
        let wants_color = if grapheme.contains('\u{fe0e}') {
            Some(false)
        } else if is_emoji(grapheme) {
            Some(policy == EmojiPolicy::Color)
        } else {
            None
        };
        let order = match wants_color {
            Some(true) => color_first.as_slice(),
            Some(false) => text_first.as_slice(),
            None => order,
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
        graphemes.push((start, font, script, bidi.levels[start].is_rtl()));
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
    for (index, &(start, font, script, rtl)) in graphemes.iter().enumerate() {
        let end = graphemes
            .get(index + 1)
            .map_or(paragraph.len(), |next| next.0);
        match runs.last_mut() {
            Some(run) if run.font == font && run.script == script && run.rtl == rtl => {
                run.range.end = end;
            }
            _ => runs.push(Run {
                range: start..end,
                font,
                script,
                rtl,
            }),
        }
    }
    runs
}

/// The rustybuzz script of a Unicode one, by its ISO 15924 code.
fn shaping_script(script: Script) -> Option<rustybuzz::Script> {
    let code: [u8; 4] = script.short_name().as_bytes().try_into().ok()?;
    rustybuzz::Script::from_iso15924_tag(ttf_parser::Tag::from_bytes(&code))
}

/// Shapes a paragraph run by run, each in its own direction and script
/// with the text around it as context, into glyphs in logical order.
fn shape_paragraph(
    faces: &Faces<'_>,
    paragraph: &str,
    bidi: &ParagraphBidiInfo<'_>,
    font_size: f32,
    order: &[usize],
    policy: EmojiPolicy,
    letter_spacing: f32,
) -> Vec<ShapedGlyph> {
    // CSS Text 3: spaced text takes no optional ligatures; required ones and cursive joins stay.
    let features = if letter_spacing == 0.0 {
        Vec::new()
    } else {
        [b"liga", b"clig", b"dlig", b"hlig"]
            .map(|tag| rustybuzz::Feature::new(ttf_parser::Tag::from_bytes(tag), 0, ..))
            .to_vec()
    };
    let mut glyphs = Vec::new();
    for run in itemize(faces, paragraph, bidi, order, policy) {
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
                // A font's own substitutions (an AAT `morx` table) may name a glyph it does not
                // have; that draws as `.notdef`.
                glyph_index: u16::try_from(info.glyph_id)
                    .ok()
                    .filter(|&glyph| glyph < face.number_of_glyphs())
                    .unwrap_or(0),
                cluster: run.range.start + usize::try_from(info.cluster).unwrap_or(0),
                advance: position.x_advance as f32 * font_size / units,
                x_offset: position.x_offset as f32 * font_size / units,
                y_offset: position.y_offset as f32 * font_size / units,
            });
        }
        // The shaper gives a right-to-left run in visual order; the paragraph is broken in logical
        // order.
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
    /// Byte offset in the paragraph of the text it draws.
    cluster: usize,
    advance: f32,
    /// Where the shaper places the glyph off its pen position, in pixels, y
    /// up: a mark on its base's anchor, for one.
    x_offset: f32,
    y_offset: f32,
}

/// The glyphs of one cluster: what a line never splits.
#[derive(Clone, Debug, PartialEq)]
struct Cluster {
    glyphs: Range<usize>,
    /// Byte offset in the paragraph of its text.
    start: usize,
    /// Its advance, its letter spacing included.
    width: f32,
    /// The letter spacing it takes after itself, in its width.
    spacing: f32,
    /// All it draws is spaces that hang at a line's end.
    hangs: bool,
    /// UAX #14 lets a line break before it.
    break_before: bool,
}

/// Groups logical-order glyphs into clusters, with where UAX #14 lets a
/// line break: except between two Hangul characters, since Korean breaks
/// between words (the tailoring UAX #14 gives for it). Each cluster but one
/// of a cursive script takes `letter_spacing` after itself.
fn clusters(glyphs: &[ShapedGlyph], paragraph: &str, letter_spacing: f32) -> Vec<Cluster> {
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

/// Splits a paragraph's clusters into lines no wider than `max_width` (none
/// when it is not positive), as cluster ranges.
///
/// A line breaks at its last opportunity that keeps it within the width;
/// what is still wider breaks between clusters. Spaces never push a line
/// over: they hang past its end.
fn break_lines(clusters: &[Cluster], max_width: f32) -> Vec<Range<usize>> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut x = 0.0_f32;
    // The latest cluster a line may start at, and the pen position there.
    let mut opportunity: Option<(usize, f32)> = None;
    for (index, cluster) in clusters.iter().enumerate() {
        if cluster.break_before && index > start {
            opportunity = Some((index, x));
        }
        // A line that ends at this cluster ends before its spacing.
        let fits = |x: f32| x + cluster.width - cluster.spacing <= max_width;
        if max_width > 0.0 && !cluster.hangs && index > start && !fits(x) {
            let (end, width) = opportunity.unwrap_or((index, x));
            lines.push(start..end);
            start = end;
            x -= width;
            opportunity = None;
            // What moved down with this cluster is wider than a line: it breaks again before the
            // cluster.
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
    /// Pen position from the line's start.
    x: f32,
    /// The shaper's placement of the glyph off its pen position, in pixels, y up.
    x_offset: f32,
    y_offset: f32,
}

/// How a glyph is drawn: its outline, as coverage the text's color fills,
/// or under [`EmojiPolicy::Color`] its color form when the font has one.
enum Drawing {
    Outline(Outline),
    Painting(color::Painting),
    Bitmap(color::Bitmap),
}

impl Drawing {
    /// The glyph's COLR painting, else its bitmap, else its outline. The
    /// painting takes the text's color, opaque, where it asks for it: the
    /// text's alpha applies to the whole glyph when it is composed.
    fn of(faces: &Faces<'_>, font: usize, glyph: u16, request: &TextRasterRequest) -> Self {
        let face = &faces.parsed[font];
        if request.emoji_policy == EmojiPolicy::Color && faces.color[font] {
            let [red, green, blue, _] = request.color;
            if let Some(painting) = color::Painting::of(face, glyph, [red, green, blue, 255]) {
                return Self::Painting(painting);
            }
            if let Some(bitmap) = color::Bitmap::of(face, glyph, request.font_size) {
                return Self::Bitmap(bitmap);
            }
        }
        Self::Outline(Outline::of(face, glyph))
    }

    /// `[x_min, y_min, x_max, y_max]` in font units of all it may draw;
    /// `None` when it draws nothing.
    fn bounds(&self) -> Option<[f32; 4]> {
        match self {
            Self::Outline(outline) => outline.bounds,
            Self::Painting(painting) => painting.bounds(),
            Self::Bitmap(bitmap) => Some(bitmap.bounds()),
        }
    }

    /// `[x_min, y_min, x_max, y_max]` in font units of what it draws: an
    /// outline's curves themselves, not their control points, a bitmap's
    /// pixels with alpha, or a painting's bounds; `None` when it draws
    /// nothing.
    fn ink_bounds(&self) -> Option<[f32; 4]> {
        match self {
            Self::Outline(outline) => outline.curve_bounds(),
            Self::Painting(painting) => painting.bounds(),
            Self::Bitmap(bitmap) => bitmap.ink_bounds(),
        }
    }

    /// A color glyph over `pixels`, premultiplied RGBA8; an outline draws
    /// nothing here.
    fn draw_color(&self, origin: [f32; 2], scale: f32, pixels: PixelBox) -> Vec<u8> {
        match self {
            Self::Outline(_) => Vec::new(),
            Self::Painting(painting) => painting.draw(origin, scale, pixels),
            Self::Bitmap(bitmap) => bitmap.draw(origin, scale, pixels),
        }
    }
}

/// A glyph's outline in font units, y up, as the font draws it.
#[derive(Default)]
struct Outline {
    segments: Vec<Segment>,
    /// `[x_min, y_min, x_max, y_max]` of every point, on the curves and off
    /// them, so holding the curves; `None` when it draws nothing.
    bounds: Option<[f32; 4]>,
}

enum Segment {
    Line([f32; 2], [f32; 2]),
    Quad([f32; 2], [f32; 2], [f32; 2]),
    Cubic([f32; 2], [f32; 2], [f32; 2], [f32; 2]),
}

/// The pixels a glyph's outline reaches, right and bottom exclusive, in
/// the layout box, y down.
#[derive(Copy, Clone, Debug)]
struct PixelBox {
    left: i64,
    top: i64,
    right: i64,
    bottom: i64,
}

impl PixelBox {
    /// Where outline `bounds` land with the glyph's origin at `origin`
    /// (layout pixels, y down) at `scale` pixels a font unit.
    #[allow(clippy::cast_possible_truncation)]
    fn of(bounds: [f32; 4], origin: [f32; 2], scale: f32) -> Self {
        let [x_min, y_min, x_max, y_max] = bounds;
        // Each edge the same expression `Outline::draw` maps a point by, so every point lands
        // inside.
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
    /// How many glyph outlines this thread has read from a face.
    static OUTLINES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl Outline {
    fn of(face: &ttf_parser::Face<'_>, glyph: u16) -> Self {
        #[cfg(test)]
        OUTLINES_READ.with(|read| read.set(read.get() + 1));
        let mut sink = OutlineSink::default();
        face.outline_glyph(GlyphId(glyph), &mut sink);
        sink.close();
        sink.outline
    }

    /// The glyph's coverage over `pixels`, row by row, with its origin at
    /// `origin` at `scale`.
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
    /// `[x_min, y_min, x_max, y_max]` of the curves themselves: their ends
    /// and where they turn along an axis. A curve stays within its control
    /// points but seldom reaches them, so this is tighter than `bounds`.
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
                        // Where the derivative, linear in t, is zero.
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

    /// The outline drawn into a `width` by `height` raster, each point where
    /// `at` puts it.
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

/// The real roots of `a t^2 + b t + c`: a cubic's turning points, where its
/// derivative is zero. A non-finite one, from a zero divisor, is no root.
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

/// Collects an outline from ttf-parser, closing every contour.
#[derive(Default)]
struct OutlineSink {
    outline: Outline,
    start: [f32; 2],
    pen: [f32; 2],
}

impl OutlineSink {
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
        let to = [x, y];
        self.include(&[self.pen, to]);
        self.outline.segments.push(Segment::Line(self.pen, to));
        self.pen = to;
    }

    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (control, to) = ([x1, y1], [x, y]);
        self.include(&[self.pen, control, to]);
        self.outline
            .segments
            .push(Segment::Quad(self.pen, control, to));
        self.pen = to;
    }

    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (first, second, to) = ([x1, y1], [x2, y2], [x, y]);
        self.include(&[self.pen, first, second, to]);
        self.outline
            .segments
            .push(Segment::Cubic(self.pen, first, second, to));
        self.pen = to;
    }

    fn close(&mut self) {
        // Exactly where it began: a contour that came back by its own last point needs no closing
        // line.
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
        && request.max_width <= 16_384.0
        && request.line_height.is_finite()
        && request.line_height > 0.0
        && request.line_height <= MAX_LINE_HEIGHT
        && request.letter_spacing.is_finite()
        && request.letter_spacing >= LETTER_SPACING_RANGE.start() * request.font_size
        && request.letter_spacing <= LETTER_SPACING_RANGE.end() * request.font_size
        && (1..=1000).contains(&request.font_weight)
        && request.content_version > 0
        && request.outline.is_none_or(|outline| {
            outline.width.is_finite() && outline.width > 0.0 && outline.width <= MAX_OUTLINE_WIDTH
        })
        && request.background.is_none_or(|background| {
            background.padding.is_finite()
                && background.padding >= 0.0
                && background.padding <= MAX_BACKGROUND_PADDING
        })
    {
        Ok(())
    } else {
        Err(TextRasterError::InvalidRequest)
    }
}

fn checked_dimension(value: f32) -> Result<u32, TextRasterError> {
    if value.is_finite() && value > 0.0 && value.ceil() <= 16_384.0 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(value.ceil() as u32)
    } else {
        Err(TextRasterError::TooLarge)
    }
}

/// Adds a glyph's coverage to the mask: where glyphs overlap, each pixel
/// keeps the most ink either gives it, so a glyph's empty corners never
/// erase its neighbour.
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

/// Adds a color glyph to the color layer, premultiplied RGBA8: where
/// glyphs overlap, the later one goes over.
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
    /// Pixels the decorations reach past the layout box on every side when
    /// no ink does: the outline's disc and the background's padding, whole.
    /// The texture's margin ([`TextTexture::layout_x`]) is this plus room
    /// for ink past the box.
    #[must_use]
    pub fn decoration_margin(&self) -> u32 {
        let (outline, padding) = decoration_extents(self);
        outline.max(padding)
    }
}

/// Pixels each decoration reaches past what it surrounds, whole: the
/// outline's disc past the ink, the background's padding past the box.
fn decoration_extents(request: &TextRasterRequest) -> (u32, u32) {
    let outline = request
        .outline
        .map_or(0, |outline| disc_extent(outline.width));
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let padding = request
        .background
        .map_or(0, |background| background.padding.ceil() as u32);
    (outline, padding)
}

/// Integer radius of the disc an outline of `width` dilates by.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn disc_extent(width: f32) -> u32 {
    (width + 0.5).ceil() as u32
}

/// `color` at `coverage`, premultiplied: `(c * alpha, alpha)` with
/// `alpha = coverage * a`, each product truncated to a byte. The pinned
/// rasters depend on this truncation.
fn premultiplied(color: [u8; 4], coverage: u8) -> [u32; 4] {
    let alpha = u32::from(coverage) * u32::from(color[3]) / 255;
    [
        u32::from(color[0]) * alpha / 255,
        u32::from(color[1]) * alpha / 255,
        u32::from(color[2]) * alpha / 255,
        alpha,
    ]
}

/// `source` over `destination`, premultiplied, rounded to the nearest byte.
fn over(source: [u32; 4], destination: [u32; 4]) -> [u32; 4] {
    let keep = 255 - source[3];
    std::array::from_fn(|channel| source[channel] + (destination[channel] * keep + 127) / 255)
}

/// The text over its outline over its line boxes, premultiplied RGBA8, all
/// in integers so every target gives the same bytes. Undecorated text is its
/// coverage in its color alone, with the color glyphs (`colors`,
/// premultiplied RGBA8) over it at the color's alpha. The outline surrounds
/// both: its band dilates the coverage and the color glyphs' alpha.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn compose(
    request: &TextRasterRequest,
    coverage: &[u8],
    colors: Option<&[u8]>,
    width: u32,
    margin: [u32; 2],
    line_boxes: &[(f32, f32, f32)],
) -> Vec<u8> {
    let outline = request.outline.map(|outline| {
        let ink = colors.map(|colors| {
            coverage
                .iter()
                .zip(colors.as_chunks::<4>().0)
                .map(|(&text, color)| text.max(color[3]))
                .collect::<Vec<_>>()
        });
        (
            outline.color,
            dilate(
                ink.as_deref().unwrap_or(coverage),
                width,
                u32::try_from(coverage.len() / width.max(1) as usize).unwrap_or(0),
                outline.width,
            ),
        )
    });
    let background = request.background.map(|background| {
        let [margin_x, margin_y] = margin.map(|margin| margin as f32);
        let padding = background.padding;
        let boxes = line_boxes
            .iter()
            .map(|&(left, line_width, top)| {
                (
                    margin_x + left - padding,
                    margin_x + left + line_width + padding,
                    margin_y + top - padding,
                    margin_y + top + request.font_size + padding,
                )
            })
            .collect::<Vec<_>>();
        (background.color, boxes)
    });
    let mut rgba8 = vec![0_u8; coverage.len() * 4];
    // The boxes that may meet the current row, `first..last`; boxes come in line order, so the
    // window only moves down.
    let (mut first, mut last) = (0, 0);
    for (index, text) in coverage.iter().copied().enumerate() {
        let mut pixel = [0_u32; 4];
        if let Some((color, boxes)) = &background {
            let x = (index % width as usize) as f32;
            let y = (index / width as usize) as f32;
            if index % width as usize == 0 {
                while first < boxes.len() && boxes[first].3 <= y {
                    first += 1;
                }
                last = last.max(first);
                while last < boxes.len() && boxes[last].2 < y + 1.0 {
                    last += 1;
                }
            }
            let covered = boxes[first..last]
                .iter()
                .map(|&(left, right, top, bottom)| {
                    let across = (right.min(x + 1.0) - left.max(x)).clamp(0.0, 1.0);
                    let down = (bottom.min(y + 1.0) - top.max(y)).clamp(0.0, 1.0);
                    across * down
                })
                .fold(0.0_f32, f32::max);
            pixel = premultiplied(*color, (covered * 255.0).round() as u8);
        }
        if let Some((color, band)) = &outline {
            pixel = over(premultiplied(*color, band[index]), pixel);
        }
        pixel = over(premultiplied(request.color, text), pixel);
        if let Some(colors) = colors {
            let color = &colors[index * 4..][..4];
            if color[3] > 0 {
                let opacity = u32::from(request.color[3]);
                pixel = over(
                    std::array::from_fn(|channel| u32::from(color[channel]) * opacity / 255),
                    pixel,
                );
            }
        }
        for (channel, value) in pixel.iter().enumerate() {
            rgba8[index * 4 + channel] = (*value).min(255) as u8;
        }
    }
    rgba8
}

/// The coverage dilated by a disc of radius `width`: each pixel takes the
/// highest coverage within the disc, weighted by how much of the disc's edge
/// pixel lies inside it.
///
/// The work follows the ink, not the texture: only the ink's bounds grown by
/// the disc can be reached. The disc's fully weighted pixels are, row by row,
/// one run centred on the column, so they are a sliding-window maximum
/// (van Herk/Gil-Werman) of a coverage row; only the antialiased rim is
/// visited tap by tap. Every pixel is the same integer maximum a tap-by-tap
/// pass computes, so the band is byte for byte the same.
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
    // Each run half-width once per coverage row, laid onto every output row whose disc has that run
    // there.
    let span = reach.right - reach.left;
    let mut window = WindowMax::default();
    let mut maxima = vec![0_u8; span];
    for (half, offsets) in &disc.runs {
        for source_y in ink.top..ink.bottom {
            let row = &coverage[source_y * width..(source_y + 1) * width];
            window.of(row, reach.left, span, *half, &mut maxima);
            for &offset in offsets {
                // The tap `offset` rows below an output row reads this row.
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
            // Output columns x read x + dx; keep both within the texture and the source within the
            // ink.
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

/// The smallest rectangle holding every pixel with coverage, if any has.
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

/// The taps of a disc of some radius, split into what a dilation reads as
/// runs and what it reads tap by tap.
struct Disc {
    /// Integer radius of the square holding the disc.
    extent: usize,
    /// Run half-widths with the row offsets whose fully weighted taps are
    /// exactly the columns `-half..=half`.
    runs: Vec<(usize, Vec<isize>)>,
    /// `(dx, dy, weight)` of each tap weighted below 255 and above 0.
    rim: Vec<(isize, isize, u32)>,
}

impl Disc {
    /// The disc of `radius`: a tap's weight is how much of it lies within
    /// `radius + 0.5` of the centre, in 255ths, rounded.
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
                    // Weight falls with distance, so a row's full taps are one run around its
                    // centre.
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

/// Scratch for sliding-window maxima of a row (van Herk/Gil-Werman): the
/// maxima from each block's start and to each block's end.
#[derive(Default)]
struct WindowMax {
    padded: Vec<u8>,
    from_start: Vec<u8>,
    to_end: Vec<u8>,
}

impl WindowMax {
    /// Into `out[i]`, the highest value of `row` over columns
    /// `left + i - half ..= left + i + half`, reading 0 off the row.
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
