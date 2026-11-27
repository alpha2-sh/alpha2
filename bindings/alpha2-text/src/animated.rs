//! Per-cluster animation states (motion and color) for text animators.
//!
//! [`TextRasterizer::layout`] reports where each cluster lies so a caller can compute states;
//! [`TextRasterizer::rasterize_animated`] draws with them. Tracking re-aligns a line but never
//! re-wraps it.
//!
//! Outlines are transformed before rasterizing, so scaled or rotated glyphs stay sharp. Each
//! cluster is drawn into its own tile and tiles combine by maximum coverage, taking color from the
//! winning cluster; strokes work the same way. Color glyphs are drawn upright and resampled
//! bilinearly in fixed point. Integer and `libm` arithmetic keep native and wasm output identical.

use super::{
    Drawing, DrawingKey, Faces, HashMap, HostTexture, HostTexturePixels, Layout, PixelBox,
    PixelLook, Premultiplication, RasterColorSpace, RequestLook, TextFill, TextRasterError,
    TextRasterRequest, TextRasterResult, TextRasterizer, TextStroke, TextTexture,
    decoration_extents, dilate, disc_extent, drawing_key, gradient, normalize_mapped, over, paint,
    point, raster_bytes_per_pixel, raster_pixels, stroke_radius, validate_request,
};
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

/// Cluster and line positions, matching [`TextRasterizer::rasterize`].
#[derive(Clone, Debug, PartialEq)]
pub struct TextLayout {
    /// Clusters (the unit of letter spacing) in logical order.
    pub clusters: Vec<ClusterInfo>,
    /// Lines, top to bottom.
    pub lines: Vec<LineInfo>,
    /// Font size in pixels.
    pub font_size: f32,
}

/// One cluster of a [`TextLayout`].
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterInfo {
    /// Byte range in the request's text.
    pub bytes: Range<usize>,
    /// Line index.
    pub line: u32,
    /// Index of its UAX #29 word, counting only words with a letter or digit; `None` for spaces
    /// and punctuation between words.
    pub word: Option<u32>,
    /// Whether it holds only white space.
    pub is_space: bool,
    /// Start of its advance in pixels from the layout box's left edge.
    pub x: f32,
    /// Advance in pixels; zero for a space hanging at a line end.
    pub advance: f32,
    /// Line baseline in pixels from the layout box's top.
    pub baseline: f32,
    /// Font size in pixels: its span's, else the request's.
    pub font_size: f32,
    /// The bounds of what it draws, strokes and a rim pixel included, when
    /// [`TextRasterizer::layout_with_ink`] reports it; `None` for a cluster that draws nothing
    /// and from [`TextRasterizer::layout`].
    pub ink: Option<[f32; 4]>,
    /// Whether it is the ellipsis a [`crate::TextClamp`] ends a cut line with. Its byte range
    /// is empty, at the cut, and it is in no word.
    pub ellipsis: bool,
}

/// One line of a [`TextLayout`].
#[derive(Clone, Debug, PartialEq)]
pub struct LineInfo {
    /// Cluster indices, in logical order.
    pub clusters: Range<usize>,
    /// Start in pixels from the layout box's left edge.
    pub left: f32,
    /// Advance excluding trailing spaces.
    pub width: f32,
    /// Baseline in pixels from the layout box's top.
    pub baseline: f32,
    /// Its largest size in pixels, at least the request's.
    pub size: f32,
    /// Its line box's top in pixels from the layout box's top; the box is the request's line
    /// height times `size` tall.
    pub box_top: f32,
}

/// A color change `base * keep + add` in premultiplied linear light. Mixing toward a target by
/// `w` is `keep = 1 - w`, `add = w * target`; mixes compose.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ColorMix {
    /// Factor on the base color, in `[0, 1]`.
    pub keep: f32,
    /// Premultiplied linear-light RGBA in `[0, 1]`.
    pub add: [f32; 4],
}

/// Largest scale of a [`ClusterState`] on either axis.
pub const MAX_CLUSTER_SCALE: f32 = 10.0;

/// The units whose clusters scale and turn about one pivot, as After Effects' Anchor Point
/// Grouping.
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub enum ClusterGrouping {
    /// Each cluster about its own pivot.
    #[default]
    Character,
    /// Each word of [`ClusterInfo::word`]; a cluster in no word is its own group.
    Word,
    /// Each line.
    Line,
    /// The whole text.
    All,
}

/// Animation state of one cluster.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ClusterState {
    /// Translation in pixels, +y down.
    pub offset: [f32; 2],
    /// Scale about its center, in `[0, MAX_CLUSTER_SCALE]`.
    pub scale: [f32; 2],
    /// Clockwise rotation about its center, in radians.
    pub rotation: f32,
    /// Multiplies fill and stroke coverage, in `[0, 1]`.
    pub opacity: f32,
    /// Fill color change; `None` keeps it.
    pub fill: Option<ColorMix>,
    /// Stroke color change; `None` keeps them.
    pub stroke: Option<ColorMix>,
    /// Extra space after the cluster in pixels, like letter spacing.
    pub tracking: f32,
    /// How much its advance widens, in `[0, MAX_CLUSTER_SCALE]`: mapped about its group's pivot,
    /// the advances of a group push the groups after it along their line, which is re-aligned.
    pub advance_scale: f32,
}

impl ClusterState {
    /// No change.
    pub const IDENTITY: Self = Self {
        offset: [0.0, 0.0],
        scale: [1.0, 1.0],
        rotation: 0.0,
        opacity: 1.0,
        fill: None,
        stroke: None,
        tracking: 0.0,
        advance_scale: 1.0,
    };

    /// Whether the state changes nothing.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::IDENTITY
    }

    fn is_valid(&self) -> bool {
        let mix = |mix: &Option<ColorMix>| {
            mix.is_none_or(|mix| {
                (0.0..=1.0).contains(&mix.keep)
                    && mix.add.iter().all(|value| (0.0..=1.0).contains(value))
            })
        };
        self.offset.iter().all(|value| value.is_finite())
            && self
                .scale
                .iter()
                .all(|value| (0.0..=MAX_CLUSTER_SCALE).contains(value))
            && self.rotation.is_finite()
            && (0.0..=1.0).contains(&self.opacity)
            && self.tracking.is_finite()
            && (0.0..=MAX_CLUSTER_SCALE).contains(&self.advance_scale)
            && mix(&self.fill)
            && mix(&self.stroke)
    }
}

/// An affine map of layout pixels: `[a, b, c, d, e, f]` sends `(x, y)` to
/// `(a x + c y + e, b x + d y + f)`.
type Affine = [f32; 6];

pub(super) const IDENTITY_AFFINE: Affine = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];

fn apply(affine: Affine, [x, y]: [f32; 2]) -> [f32; 2] {
    [
        affine[0] * x + affine[2] * y + affine[4],
        affine[1] * x + affine[3] * y + affine[5],
    ]
}

/// Scales and rotates about `pivot`, then translates by `offset`.
fn cluster_affine(state: &ClusterState, pivot: [f32; 2]) -> Affine {
    let (sine, cosine) = (libm::sinf(state.rotation), libm::cosf(state.rotation));
    let [sx, sy] = state.scale;
    // Rotation clockwise on screen, y down: x' = x cos - y sin, y' = x sin + y cos.
    let (a, b, c, d) = (cosine * sx, sine * sx, -sine * sy, cosine * sy);
    let [px, py] = pivot;
    [
        a,
        b,
        c,
        d,
        px - a * px - c * py + state.offset[0],
        py - b * px - d * py + state.offset[1],
    ]
}

fn invert(affine: Affine) -> Option<Affine> {
    let determinant = affine[0] * affine[3] - affine[1] * affine[2];
    if determinant.abs() <= f32::MIN_POSITIVE {
        return None;
    }
    let (a, b, c, d) = (
        affine[3] / determinant,
        -affine[1] / determinant,
        -affine[2] / determinant,
        affine[0] / determinant,
    );
    Some([
        a,
        b,
        c,
        d,
        -(a * affine[4] + c * affine[5]),
        -(b * affine[4] + d * affine[5]),
    ])
}

/// One cluster's coverage and color glyphs over a pixel rectangle (right and bottom exclusive)
/// that may extend past the layout box.
struct Tile {
    left: i64,
    top: i64,
    width: usize,
    height: usize,
    coverage: Vec<u8>,
    /// Premultiplied RGBA8, once a color glyph is drawn into it.
    colors: Option<Vec<u8>>,
}

impl Tile {
    fn empty() -> Self {
        Self {
            left: 0,
            top: 0,
            width: 0,
            height: 0,
            coverage: Vec::new(),
            colors: None,
        }
    }

    fn grow(&mut self, bounds: PixelBox) {
        if self.width == 0 || self.height == 0 {
            *self = Self {
                left: bounds.left,
                top: bounds.top,
                width: usize::try_from(bounds.right - bounds.left).unwrap_or(0),
                height: usize::try_from(bounds.bottom - bounds.top).unwrap_or(0),
                coverage: Vec::new(),
                colors: None,
            };
            self.coverage = vec![0; self.width * self.height];
            return;
        }
        let right = (self.left + i64::try_from(self.width).unwrap_or(0)).max(bounds.right);
        let bottom = (self.top + i64::try_from(self.height).unwrap_or(0)).max(bounds.bottom);
        let (left, top) = (self.left.min(bounds.left), self.top.min(bounds.top));
        if left == self.left
            && top == self.top
            && right == self.left + i64::try_from(self.width).unwrap_or(0)
            && bottom == self.top + i64::try_from(self.height).unwrap_or(0)
        {
            return;
        }
        let width = usize::try_from(right - left).unwrap_or(0);
        let height = usize::try_from(bottom - top).unwrap_or(0);
        let (dx, dy) = (
            usize::try_from(self.left - left).unwrap_or(0),
            usize::try_from(self.top - top).unwrap_or(0),
        );
        let mut coverage = vec![0; width * height];
        let mut colors = self.colors.as_ref().map(|_| vec![0; width * height * 4]);
        for row in 0..self.height {
            let to = (row + dy) * width + dx;
            coverage[to..to + self.width]
                .copy_from_slice(&self.coverage[row * self.width..(row + 1) * self.width]);
            if let (Some(colors), Some(held)) = (&mut colors, &self.colors) {
                colors[to * 4..(to + self.width) * 4]
                    .copy_from_slice(&held[row * self.width * 4..(row + 1) * self.width * 4]);
            }
        }
        *self = Self {
            left,
            top,
            width,
            height,
            coverage,
            colors,
        };
    }

    fn index(&self, x: i64, y: i64) -> Option<usize> {
        let (x, y) = (
            usize::try_from(x - self.left).ok()?,
            usize::try_from(y - self.top).ok()?,
        );
        (x < self.width && y < self.height).then_some(y * self.width + x)
    }

    fn bounds(&self) -> Option<PixelBox> {
        (self.width > 0 && self.height > 0).then(|| PixelBox {
            left: self.left,
            top: self.top,
            right: self.left + i64::try_from(self.width).unwrap_or(0),
            bottom: self.top + i64::try_from(self.height).unwrap_or(0),
        })
    }
}

/// Pixel box of a glyph's font-unit `bounds` after placement and `affine`.
#[allow(clippy::cast_possible_truncation)]
fn mapped_box(bounds: [f32; 4], origin: [f32; 2], scale: f32, affine: Affine) -> PixelBox {
    let [x_min, y_min, x_max, y_max] = bounds;
    let corners = [
        [x_min, y_min],
        [x_max, y_min],
        [x_min, y_max],
        [x_max, y_max],
    ]
    .map(|[x, y]| apply(affine, [origin[0] + x * scale, origin[1] - y * scale]));
    let (mut left, mut top, mut right, mut bottom) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for [x, y] in corners {
        (left, top, right, bottom) = (left.min(x), top.min(y), right.max(x), bottom.max(y));
    }
    PixelBox {
        left: left.floor() as i64,
        top: top.floor() as i64,
        right: right.ceil() as i64,
        bottom: bottom.ceil() as i64,
    }
}

/// Resamples an upright color glyph into `target` through `affine`, bilinear with 8-bit weights.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::many_single_char_names
)]
fn resample(rgba8: &[u8], source: PixelBox, target: PixelBox, inverse: Affine) -> Vec<u8> {
    let source_width = usize::try_from(source.right - source.left).unwrap_or(0);
    let source_height = usize::try_from(source.bottom - source.top).unwrap_or(0);
    let width = usize::try_from(target.right - target.left).unwrap_or(0);
    let height = usize::try_from(target.bottom - target.top).unwrap_or(0);
    let mut out = vec![0_u8; width * height * 4];
    let texel = |x: i64, y: i64| -> [u32; 4] {
        if x < 0 || y < 0 || x >= source_width as i64 || y >= source_height as i64 {
            return [0; 4];
        }
        let at = (y as usize * source_width + x as usize) * 4;
        std::array::from_fn(|channel| u32::from(rgba8[at + channel]))
    };
    for row in 0..height {
        for column in 0..width {
            let [x, y] = apply(
                inverse,
                [
                    (target.left + column as i64) as f32 + 0.5,
                    (target.top + row as i64) as f32 + 0.5,
                ],
            );
            // Texel space, with texel centres at whole numbers.
            let (u, v) = (x - source.left as f32 - 0.5, y - source.top as f32 - 0.5);
            let (u0, v0) = (u.floor(), v.floor());
            let fx = ((u - u0) * 256.0) as u32;
            let fy = ((v - v0) * 256.0) as u32;
            let (u0, v0) = (u0 as i64, v0 as i64);
            let [a, b, c, d] = [
                texel(u0, v0),
                texel(u0 + 1, v0),
                texel(u0, v0 + 1),
                texel(u0 + 1, v0 + 1),
            ];
            let at = (row * width + column) * 4;
            for channel in 0..4 {
                let top = a[channel] * (256 - fx) + b[channel] * fx;
                let bottom = c[channel] * (256 - fx) + d[channel] * fx;
                let value = (top * (256 - fy) + bottom * fy + 32_768) >> 16;
                out[at + channel] = value.min(255) as u8;
            }
        }
    }
    out
}

fn mixed(color: [u8; 4], mix: &ColorMix) -> [u8; 4] {
    let base = gradient::premultiplied_linear(color.map(|value| f32::from(value) / 255.0));
    gradient::stored_rgba8(std::array::from_fn(|channel| {
        base[channel] * mix.keep + mix.add[channel]
    }))
}

/// Per-pixel color ownership: each pixel takes its colors from the cluster that gives it coverage.
struct ClusterLook<'a> {
    looks: Vec<RequestLook<'a>>,
    cluster_looks: Vec<u16>,
    states: &'a [ClusterState],
    /// Owning cluster per pixel, `u32::MAX` for none.
    fill_owner: Vec<u32>,
    band_owner: Vec<Vec<u32>>,
    /// The cluster whose color glyph was drawn last at each pixel.
    color_owner: Vec<u32>,
    solid: Vec<Option<[u8; 4]>>,
    strokes: Vec<Vec<[u8; 4]>>,
}

impl PixelLook for ClusterLook<'_> {
    fn fill(&self, index: usize, point: [f32; 2]) -> [u8; 4] {
        let owner = self.fill_owner[index] as usize;
        let Some(state) = self.states.get(owner) else {
            return self.looks[0].fill(index, point);
        };
        let look = &self.looks[usize::from(self.cluster_looks[owner])];
        match (&look.gradient, state.fill) {
            (Some(gradient), Some(mix)) => {
                let base = gradient.linear_at(point);
                gradient::stored_rgba8(std::array::from_fn(|channel| {
                    base[channel] * mix.keep + mix.add[channel]
                }))
            }
            (None, Some(_)) => self.solid[owner].unwrap_or(look.foreground),
            (_, None) => look.fill(index, point),
        }
    }

    fn band(&self, stroke: usize, index: usize) -> [u8; 4] {
        let owner = self.band_owner[stroke][index] as usize;
        self.strokes[stroke]
            .get(owner)
            .copied()
            .unwrap_or_else(|| self.looks[0].band(stroke, index))
    }

    /// Opacity of the owning cluster: fill, else color glyph, else innermost band.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn fade(&self, index: usize) -> Option<u32> {
        let owner = [self.fill_owner[index], self.color_owner[index]]
            .into_iter()
            .chain(self.band_owner.iter().map(|owners| owners[index]))
            .find(|&owner| owner != u32::MAX)?;
        let opacity = (self.states.get(owner as usize)?.opacity * 255.0).round() as u32;
        (opacity < 255).then_some(opacity)
    }

    fn color_alpha(&self, index: usize) -> u32 {
        let owner = self.color_owner[index] as usize;
        let look = self
            .cluster_looks
            .get(owner)
            .map_or(&self.looks[0], |&look| &self.looks[usize::from(look)]);
        u32::from(look.foreground[3])
    }
}

/// Writes `tile`'s coverage into `plane` where it is larger (ties go to the later cluster) and
/// records `cluster` as the owner.
fn place_max(
    plane: (&mut [u8], &mut [u32], u32),
    margin: [u32; 2],
    tile: &Tile,
    coverage: &[u8],
    cluster: u32,
) {
    let (plane, owners, width) = plane;
    let height = plane.len() / width.max(1) as usize;
    for row in 0..tile.height {
        let y = tile.top + i64::try_from(row).unwrap_or(0) + i64::from(margin[1]);
        let Ok(y) = usize::try_from(y) else { continue };
        if y >= height {
            continue;
        }
        for column in 0..tile.width {
            let value = coverage[row * tile.width + column];
            if value == 0 {
                continue;
            }
            let x = tile.left + i64::try_from(column).unwrap_or(0) + i64::from(margin[0]);
            let Ok(x) = usize::try_from(x) else { continue };
            if x >= width as usize {
                continue;
            }
            let at = y * width as usize + x;
            if value >= plane[at] {
                plane[at] = value;
                owners[at] = cluster;
            }
        }
    }
}

impl TextRasterizer {
    /// Lays out `request`'s clusters and lines as [`Self::rasterize`] would.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::rasterize`].
    pub fn layout(&self, request: &TextRasterRequest) -> Result<TextLayout, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let (faces, shaping) = self.prepare(request)?;
        let layout = Self::lay_out(&faces, &shaping, request)?;
        Ok(text_layout(&layout, &shaping, request))
    }

    /// [`Self::layout`] with each cluster's [`ClusterInfo::ink`]: the bounds of its glyphs as
    /// drawn (outlines, sheared ones included, or color glyphs' placed boxes) grown by its
    /// strokes' reach and a rim pixel.
    ///
    /// # Errors
    /// Returns the same errors as [`Self::rasterize`].
    pub fn layout_with_ink(
        &self,
        request: &TextRasterRequest,
    ) -> Result<TextLayout, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let (faces, shaping) = self.prepare(request)?;
        let layout = Self::lay_out(&faces, &shaping, request)?;
        let mut inks = vec![None; layout.clusters.len()];
        layout.each_glyph(
            &faces,
            &shaping,
            request,
            &mut HashMap::new(),
            |drawing, origin, scale, glyph| {
                if let Some([x_min, y_min, x_max, y_max]) = drawing.ink_bounds() {
                    super::include(
                        &mut inks[glyph.cluster],
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
        for (ink, span) in inks.iter_mut().zip(&layout.clusters) {
            let look = &shaping.looks[usize::from(span.look)];
            let band = stroke_radius(&look.strokes).map_or(0.0, |radius| radius + 1.0);
            *ink = ink.map(|bounds| super::grown(bounds, band));
        }
        let mut reported = text_layout(&layout, &shaping, request);
        for (cluster, ink) in reported.clusters.iter_mut().zip(inks) {
            cluster.ink = ink;
        }
        Ok(reported)
    }

    /// [`Self::rasterize`] with one state per [`Self::layout`] cluster, in logical order. Identity
    /// states match [`Self::rasterize`] exactly. Results are cached by content version, so give
    /// each set of states its own version.
    ///
    /// # Errors
    /// Returns [`TextRasterError::InvalidRequest`] when the state count or a value is out of
    /// range, otherwise what [`Self::rasterize`] returns.
    pub fn rasterize_animated(
        &mut self,
        request: &TextRasterRequest,
        states: &[ClusterState],
    ) -> Result<Arc<TextRasterResult>, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        if !states.iter().all(ClusterState::is_valid) {
            return Err(TextRasterError::InvalidRequest);
        }
        if states.iter().all(ClusterState::is_identity) {
            let result = self.rasterize(request)?;
            let clusters = self.layout(request)?.clusters.len();
            return if states.len() == clusters {
                Ok(result)
            } else {
                Err(TextRasterError::InvalidRequest)
            };
        }
        if let Some(cached) = self.cache.get(request.content_version) {
            return Ok(cached);
        }
        let mut tiles = std::mem::take(&mut self.tiles);
        let result = self.rasterize_states(request, states, &mut tiles);
        self.tiles = tiles;
        let result = Arc::new(result?);
        self.cache.insert(request.content_version, &result);
        Ok(result)
    }

    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    pub(super) fn rasterize_states(
        &self,
        request: &TextRasterRequest,
        states: &[ClusterState],
        cache: &mut TileCache,
    ) -> Result<TextRasterResult, TextRasterError> {
        let (faces, shaping) = self.prepare(request)?;
        let layout = Self::lay_out(&faces, &shaping, request)?;
        if states.len() != layout.clusters.len() {
            return Err(TextRasterError::InvalidRequest);
        }
        let (box_width, box_height) = (layout.box_width, layout.box_height);
        let mut placed = vec![(Vec::<Placed>::new(), [0.0_f32; 6]); states.len()];
        let mut affines = vec![IDENTITY_AFFINE; states.len()];
        let mut decorations = paint::LinePaint::default();
        let mut drawings = HashMap::<DrawingKey, Drawing>::new();
        let (mut ink_ascent, mut ink_descent) = (layout.ascent, layout.descent);
        let pushed = Pushed::of(&layout, &shaping, request, states);
        for (index, line) in layout.lines.iter().enumerate() {
            let baseline = layout.baseline(index);
            let (shifts, offset) = (&pushed.shifts[index], pushed.offsets[index]);
            let mut extents = HashMap::<usize, (f32, f32)>::new();
            for (glyph, shift) in line.glyphs.iter().zip(shifts) {
                let left = glyph.x + shift + offset + pushed.moves[glyph.cluster];
                let extent = extents.entry(glyph.cluster).or_insert((left, left));
                *extent = (extent.0.min(left), extent.1.max(left + glyph.advance));
            }
            for (glyph, shift) in line.glyphs.iter().zip(shifts) {
                let affine = cluster_affine(&states[glyph.cluster], pushed.pivots[glyph.cluster]);
                affines[glyph.cluster] = affine;
                let key = drawing_key(&shaping, glyph);
                let drawing = drawings
                    .entry(key)
                    .or_insert_with(|| Drawing::of(&faces, key, request.emoji_policy));
                let Some(bounds) = drawing.bounds() else {
                    continue;
                };
                let scale = glyph.size / faces.units_per_em(glyph.font);
                ink_ascent = ink_ascent.max(bounds[3] * scale + glyph.y_offset);
                ink_descent = ink_descent.max(-(bounds[1] * scale + glyph.y_offset));
                let slot = &mut placed[glyph.cluster];
                slot.1 = affine;
                slot.0.push(Placed {
                    key,
                    origin: [
                        glyph.x + shift + glyph.x_offset + offset + pushed.moves[glyph.cluster],
                        baseline - glyph.y_offset,
                    ],
                    scale,
                });
            }
            decorations.add_line(
                &faces,
                &shaping,
                &layout,
                index,
                |cluster| extents[&cluster],
                |cluster| affines[cluster],
            );
        }
        let cluster_strokes = |cluster: usize| -> &[TextStroke] {
            &shaping.looks[usize::from(layout.clusters[cluster].look)].strokes
        };
        let decorations = decorations
            .decorations
            .iter()
            .map(|decoration| {
                let line = layout.clusters[decoration.owner].line;
                let baseline = layout.baseline(line);
                ink_ascent = ink_ascent.max(baseline - decoration.rect[1]);
                ink_descent = ink_descent.max(decoration.rect[3] - baseline);
                let strokes = cluster_strokes(decoration.owner);
                let drawn = draw_box(decoration.rect, decoration.affine, strokes);
                (decoration.owner, (Arc::new(drawn), [0, 0]))
            })
            .collect::<Vec<_>>();
        // Tiles depend only on the glyphs, the linear part of the map and the subpixel position,
        // so they can be cached. Every tile is sized here, before any is drawn.
        let look_keys = shaping
            .looks
            .iter()
            .map(|look| look_key(request, &look.strokes))
            .collect::<Vec<_>>();
        let mut planned = Vec::with_capacity(states.len());
        for (cluster, (glyphs, affine)) in placed.iter().enumerate() {
            let Some(first) = glyphs.first() else {
                planned.push(None);
                continue;
            };
            let [x, y] = apply(*affine, first.origin);
            let whole = [x.floor(), y.floor()];
            let local = [
                affine[0],
                affine[1],
                affine[2],
                affine[3],
                affine[4] - whole[0],
                affine[5] - whole[1],
            ];
            let (bounds, color) = cluster_box(&drawings, glyphs, local);
            planned.push(Some(PlannedCluster {
                key: cluster_key(
                    look_keys[usize::from(layout.clusters[cluster].look)],
                    &faces,
                    glyphs,
                    local,
                ),
                whole: [whole[0] as i64, whole[1] as i64],
                local,
                bounds,
                color,
            }));
        }
        // Same symmetric margin as the static raster, with room for strokes.
        let (_, padding) = decoration_extents(request);
        let outline = shaping
            .looks
            .iter()
            .filter_map(|look| stroke_radius(&look.strokes).map(disc_extent))
            .max()
            .unwrap_or(0);
        let band_count = shaping
            .looks
            .iter()
            .map(|look| look.strokes.len())
            .max()
            .unwrap_or(0);
        let (mut overflow_x, mut overflow_y) = (0_i64, 0_i64);
        for bounds in planned
            .iter()
            .flatten()
            .filter_map(|cluster| cluster.bounds.map(|bounds| bounds.moved(cluster.whole)))
            .chain(
                decorations
                    .iter()
                    .filter_map(|(_, (drawn, _))| drawn.tile.bounds()),
            )
        {
            overflow_x = overflow_x
                .max(bounds.left.saturating_neg())
                .max(bounds.right.saturating_sub(i64::from(box_width)));
            overflow_y = overflow_y
                .max(bounds.top.saturating_neg())
                .max(bounds.bottom.saturating_sub(i64::from(box_height)));
        }
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
        // Two owner planes of `u32`, and an owner plane per stroke band.
        let colors = planned.iter().flatten().any(|cluster| cluster.color);
        let owners = 8 + 4 * band_count as u64;
        let pixel_count = raster_pixels(
            width,
            height,
            raster_bytes_per_pixel(request, band_count, colors) + owners,
        )?;
        if u64::from(width) * u64::from(height) > MAX_ANIMATED_PIXELS {
            return Err(TextRasterError::TooLarge);
        }
        let mut drawing = std::collections::HashSet::new();
        let mut tile_bytes = 0_u64;
        for (index, cluster) in planned.iter().enumerate() {
            let Some(cluster) = cluster else {
                continue;
            };
            let strokes = cluster_strokes(index);
            let reach = stroke_radius(strokes).map_or(0, disc_extent);
            if !cache.contains(cluster.key) && drawing.insert(cluster.key) {
                tile_bytes = tile_bytes.saturating_add(cluster.drawing_bytes(strokes.len(), reach));
            }
        }
        if tile_bytes > MAX_REQUEST_TILE_BYTES {
            return Err(TextRasterError::TooLarge);
        }
        let mut tiles = Vec::with_capacity(states.len());
        for cluster in &planned {
            let Some(cluster) = cluster else {
                tiles.push(None);
                continue;
            };
            let drawn = if let Some(drawn) = cache.get(cluster.key) {
                drawn
            } else {
                let glyphs = &placed[tiles.len()].0;
                let drawn = Arc::new(draw_cluster(
                    cluster_strokes(tiles.len()),
                    &drawings,
                    glyphs,
                    cluster.local,
                ));
                debug_assert_eq!(
                    drawn.tile.bounds().map(|bounds| [
                        bounds.left,
                        bounds.top,
                        bounds.right,
                        bounds.bottom
                    ]),
                    cluster.bounds.map(|bounds| [
                        bounds.left,
                        bounds.top,
                        bounds.right,
                        bounds.bottom
                    ]),
                );
                cache.insert(cluster.key, &drawn);
                drawn
            };
            tiles.push(Some((drawn, cluster.whole)));
        }
        let margins = [margin_x, margin_y];
        let mut coverage = vec![0_u8; pixel_count];
        let mut fill_owner = vec![u32::MAX; pixel_count];
        let mut color_owner = vec![u32::MAX; pixel_count];
        let mut colors: Option<Vec<u8>> = None;
        let mut bands = vec![vec![0_u8; pixel_count]; band_count];
        let mut band_owner = vec![vec![u32::MAX; pixel_count]; band_count];
        for (cluster, (drawn, at)) in tiles
            .iter()
            .enumerate()
            .filter_map(|(cluster, tile)| tile.as_ref().map(|tile| (cluster, tile)))
            .chain(decorations.iter().map(|(owner, tile)| (*owner, tile)))
        {
            let cluster = u32::try_from(cluster).map_err(|_| TextRasterError::TooLarge)?;
            let tile = drawn.tile.moved(*at);
            place_max(
                (&mut coverage, &mut fill_owner, width),
                margins,
                &tile,
                &drawn.tile.coverage,
                cluster,
            );
            if let Some(held) = &drawn.tile.colors {
                let plane = colors.get_or_insert_with(|| vec![0; pixel_count * 4]);
                for row in 0..tile.height {
                    for column in 0..tile.width {
                        let source = &held[(row * tile.width + column) * 4..][..4];
                        if source[3] == 0 {
                            continue;
                        }
                        let x = tile.left + column as i64 + i64::from(margin_x);
                        let y = tile.top + row as i64 + i64::from(margin_y);
                        if x < 0 || y < 0 || x >= i64::from(width) || y >= i64::from(height) {
                            continue;
                        }
                        let pixel = y as usize * width as usize + x as usize;
                        color_owner[pixel] = cluster;
                        let at = pixel * 4;
                        let slot = &mut plane[at..at + 4];
                        let blended = over(
                            std::array::from_fn(|channel| u32::from(source[channel])),
                            std::array::from_fn(|channel| u32::from(slot[channel])),
                        );
                        for (byte, value) in slot.iter_mut().zip(blended) {
                            *byte = u8::try_from(value.min(255)).unwrap_or(u8::MAX);
                        }
                    }
                }
            }
            let padded = drawn.padded.moved(*at);
            for ((band, owners), dilated) in bands
                .iter_mut()
                .zip(band_owner.iter_mut())
                .zip(&drawn.bands)
            {
                place_max((band, owners, width), margins, &padded, dilated, cluster);
            }
        }
        let line_boxes = layout.line_boxes(request.align);
        let run_boxes = layout.paint(&faces, &shaping, request.align).boxes;
        let size = [layout.width, layout.height];
        let looks = shaping
            .looks
            .iter()
            .map(|look| RequestLook::painting(&look.fill, &look.strokes, size))
            .collect::<Vec<_>>();
        let cluster_looks = layout
            .clusters
            .iter()
            .map(|cluster| cluster.look)
            .collect::<Vec<_>>();
        let solid = states
            .iter()
            .zip(&cluster_looks)
            .map(
                |(state, &look)| match &shaping.looks[usize::from(look)].fill {
                    TextFill::Solid(color) => state.fill.map(|mix| mixed(*color, &mix)),
                    TextFill::Gradient(_) => None,
                },
            )
            .collect();
        let strokes = (0..band_count)
            .map(|band| {
                states
                    .iter()
                    .zip(&cluster_looks)
                    .map(|(state, &look)| {
                        shaping.looks[usize::from(look)].strokes.get(band).map_or(
                            [0; 4],
                            |stroke| {
                                state
                                    .stroke
                                    .map_or(stroke.color, |mix| mixed(stroke.color, &mix))
                            },
                        )
                    })
                    .collect()
            })
            .collect();
        let look = ClusterLook {
            looks,
            cluster_looks,
            states,
            fill_owner,
            band_owner,
            color_owner,
            solid,
            strokes,
        };
        let (rgba8, background) = super::compose(
            request,
            &coverage,
            colors.as_deref(),
            width,
            margins,
            &line_boxes,
            &run_boxes,
            &bands,
            &look,
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
}

/// Where an animated text's clusters go along their lines: tracked, then pushed apart by their
/// advance scales, group by group in visual order, and the line re-aligned.
struct Pushed {
    /// Per line, each glyph's tracking shift, in visual order.
    shifts: Vec<Vec<f32>>,
    /// Per line, where it starts once tracked and pushed.
    offsets: Vec<f32>,
    /// Per cluster, how far its group moved along its line, from its tracked place at the
    /// line's final start.
    moves: Vec<f32>,
    /// Per cluster, the point it scales and turns about.
    pivots: Vec<[f32; 2]>,
}

impl Pushed {
    fn of(
        layout: &Layout,
        shaping: &super::runs::Shaping,
        request: &TextRasterRequest,
        states: &[ClusterState],
    ) -> Self {
        let tracked = Tracked::of(layout, request, states);
        let groups = group_ids(layout, shaping, request);
        let mut boxes = vec![GroupBox::default(); groups.iter().max().map_or(0, |last| last + 1)];
        for (cluster, extent) in tracked.extents.iter().enumerate() {
            if let Some(extent) = extent {
                let line = layout.clusters[cluster].line;
                let size = tracked.sizes[cluster];
                boxes[groups[cluster]].add(*extent, line, layout.baseline(line), size);
            }
        }
        let group_pivots = boxes
            .iter()
            .map(|group| group.pivot(request.animator_pivot))
            .collect::<Vec<_>>();
        let mut moves = vec![0.0_f32; states.len()];
        let mut pivots = groups
            .iter()
            .map(|&group| group_pivots[group])
            .collect::<Vec<_>>();
        let mut offsets = Vec::with_capacity(layout.lines.len());
        for (line, &(tracked_offset, shift)) in layout.lines.iter().zip(&tracked.lines) {
            let (group_moves, pushed) =
                push_line(line, &tracked.extents, &groups, &group_pivots, states);
            let offset = if pushed == 0.0 {
                tracked_offset
            } else {
                line_start(layout, line.rtl, line.width + shift + pushed, request)
            };
            for glyph in &line.glyphs {
                let (cluster, group) = (glyph.cluster, groups[glyph.cluster]);
                let group_move = group_moves.get(&group).copied().unwrap_or(0.0);
                moves[cluster] = group_move;
                pivots[cluster][0] =
                    group_pivots[group][0] + group_move + (offset - tracked_offset);
            }
            offsets.push(offset);
        }
        Self {
            shifts: tracked.shifts,
            offsets,
            moves,
            pivots,
        }
    }
}

/// An animated text's clusters placed with their tracking, before any push.
struct Tracked {
    /// Per line, each glyph's tracking shift, in visual order.
    shifts: Vec<Vec<f32>>,
    /// Per line, where it starts once tracked, and its tracking in all.
    lines: Vec<(f32, f32)>,
    /// Per cluster, its advance's extent from the layout box's left edge; `None` without glyphs.
    extents: Vec<Option<(f32, f32)>>,
    /// Per cluster, its glyphs' size.
    sizes: Vec<f32>,
}

impl Tracked {
    fn of(layout: &Layout, request: &TextRasterRequest, states: &[ClusterState]) -> Self {
        let mut tracked = Self {
            shifts: Vec::with_capacity(layout.lines.len()),
            lines: Vec::with_capacity(layout.lines.len()),
            extents: vec![None; states.len()],
            sizes: vec![0.0; states.len()],
        };
        for line in &layout.lines {
            // Tracking goes after each cluster except the line's last, in visual order.
            let mut shifts = Vec::with_capacity(line.glyphs.len());
            let mut shift = 0.0_f32;
            let mut previous: Option<usize> = None;
            for glyph in &line.glyphs {
                if let Some(before) = previous.filter(|&before| before != glyph.cluster) {
                    shift += states[before].tracking;
                }
                previous = Some(glyph.cluster);
                shifts.push(shift);
            }
            let offset = line_start(layout, line.rtl, line.width + shift, request);
            for (glyph, shift) in line.glyphs.iter().zip(&shifts) {
                let left = glyph.x + shift + offset;
                let extent = tracked.extents[glyph.cluster].get_or_insert((left, left));
                *extent = (extent.0.min(left), extent.1.max(left + glyph.advance));
                tracked.sizes[glyph.cluster] = glyph.size;
            }
            tracked.shifts.push(shifts);
            tracked.lines.push((offset, shift));
        }
        tracked
    }
}

/// How far each group on `line` moves, in visual order, as the groups before it grow by their
/// clusters' advance scales about their pivots; and how much the line grew in all.
fn push_line(
    line: &super::LaidLine,
    extents: &[Option<(f32, f32)>],
    groups: &[usize],
    pivots: &[[f32; 2]],
    states: &[ClusterState],
) -> (HashMap<usize, f32>, f32) {
    let mut order = Vec::<usize>::new();
    let mut spans = HashMap::<usize, [f32; 4]>::new();
    let mut seen = std::collections::HashSet::new();
    for glyph in &line.glyphs {
        let cluster = glyph.cluster;
        let Some((left, right)) = extents[cluster].filter(|_| seen.insert(cluster)) else {
            continue;
        };
        let group = groups[cluster];
        let scale = states[cluster].advance_scale;
        let mapped = if scale - 1.0 == 0.0 {
            (left, right)
        } else {
            let pivot = pivots[group][0];
            (
                pivot + (left - pivot) * scale,
                pivot + (right - pivot) * scale,
            )
        };
        let span = spans.entry(group).or_insert_with(|| {
            order.push(group);
            [left, right, mapped.0, mapped.1]
        });
        *span = [
            span[0].min(left),
            span[1].max(right),
            span[2].min(mapped.0),
            span[3].max(mapped.1),
        ];
    }
    let mut pushed = 0.0_f32;
    let mut moves = HashMap::<usize, f32>::new();
    for group in order {
        let [left, right, mapped_left, mapped_right] = spans[&group];
        moves.insert(group, pushed + (left - mapped_left));
        pushed += (left - mapped_left) + (mapped_right - right);
    }
    (moves, pushed)
}

/// Each cluster's group under the request's grouping, numbered from 0.
fn group_ids(
    layout: &Layout,
    shaping: &super::runs::Shaping,
    request: &TextRasterRequest,
) -> Vec<usize> {
    let count = layout.clusters.len();
    match request.animator_grouping {
        ClusterGrouping::Character => (0..count).collect(),
        ClusterGrouping::Line => layout.clusters.iter().map(|cluster| cluster.line).collect(),
        ClusterGrouping::All => vec![0; count],
        ClusterGrouping::Word => {
            let clusters = text_layout(layout, shaping, request).clusters;
            let words = clusters
                .iter()
                .filter_map(|cluster| cluster.word)
                .max()
                .map_or(0, |last| last as usize + 1);
            clusters
                .iter()
                .enumerate()
                .map(|(index, cluster)| cluster.word.map_or(words + index, |word| word as usize))
                .collect()
        }
    }
}

/// A group's box: the union of its clusters' advances by their em boxes.
#[derive(Copy, Clone, Debug, Default)]
struct GroupBox {
    /// `[left, right, top, bottom]`, once a cluster is added.
    bounds: Option<[f32; 4]>,
    /// Its one line's baseline and largest size, while it lies on one line.
    line: Option<(usize, f32, f32)>,
    clusters: usize,
}

impl GroupBox {
    fn add(&mut self, (left, right): (f32, f32), line: usize, baseline: f32, size: f32) {
        let (top, bottom) = (baseline - 0.8 * size, baseline + 0.2 * size);
        self.bounds = Some(self.bounds.map_or([left, right, top, bottom], |held| {
            [
                held[0].min(left),
                held[1].max(right),
                held[2].min(top),
                held[3].max(bottom),
            ]
        }));
        self.line = match self.line {
            None if self.clusters == 0 => Some((line, baseline, size)),
            Some((held, baseline, largest)) if held == line => {
                Some((line, baseline, largest.max(size)))
            }
            _ => None,
        };
        self.clusters += 1;
    }

    /// The point at fractions `[x, y]` of the box. On one line every em box shares the baseline,
    /// so the box is the largest one's.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a point between two f32 values"
    )]
    fn pivot(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        let Some([left, right, top, bottom]) = self.bounds else {
            return [0.0; 2];
        };
        let across =
            (f64::from(left) * f64::from(1.0 - x) + f64::from(right) * f64::from(x)) as f32;
        let down = match self.line {
            Some((_, baseline, size)) => baseline - size * (0.8 - y),
            None => top + (bottom - top) * y,
        };
        [across, down]
    }
}

/// Most pixels an animated raster may have. Its planes and the owner planes take 17 to 37 bytes
/// per pixel, about twice a static raster.
const MAX_ANIMATED_PIXELS: u64 = 1 << 25;

/// Most bytes the tiles one request draws (those not cached) may take while drawn.
const MAX_REQUEST_TILE_BYTES: u64 = 256 << 20;

/// Most bytes the tile cache holds.
const MAX_TILE_CACHE_BYTES: usize = 64 << 20;

/// A cluster's tile key, placement and box, known before the tile is drawn.
struct PlannedCluster {
    key: u64,
    whole: [i64; 2],
    local: Affine,
    /// The box [`draw_cluster`] gives the tile, in the frame offset by `whole`.
    bounds: Option<PixelBox>,
    /// Whether a glyph is a color glyph.
    color: bool,
}

impl PlannedCluster {
    /// Bytes drawing the tile takes: coverage and the outline accumulator (4 bytes) per pixel,
    /// color glyphs and their resampled copy (8 more), and over the box padded by `reach` the
    /// ink plane and one band per stroke.
    fn drawing_bytes(&self, strokes: usize, reach: u32) -> u64 {
        let Some(bounds) = self.bounds else {
            return 0;
        };
        let side = |from: i64, to: i64| u64::try_from(to.saturating_sub(from)).unwrap_or(0);
        let (width, height) = (
            side(bounds.left, bounds.right),
            side(bounds.top, bounds.bottom),
        );
        let pixels = width.saturating_mul(height);
        let padded = width
            .saturating_add(2 * u64::from(reach))
            .saturating_mul(height.saturating_add(2 * u64::from(reach)));
        let per_pixel = 5 + if self.color { 8 } else { 0 };
        let planes = if strokes == 0 { 0 } else { strokes as u64 + 1 };
        pixels
            .saturating_mul(per_pixel)
            .saturating_add(padded.saturating_mul(planes))
    }
}

/// The tile box [`draw_cluster`] draws `glyphs` into through `affine`, and whether any has color.
fn cluster_box(
    drawings: &HashMap<DrawingKey, Drawing>,
    glyphs: &[Placed],
    affine: Affine,
) -> (Option<PixelBox>, bool) {
    let mut union: Option<PixelBox> = None;
    let mut color = false;
    for glyph in glyphs {
        let drawing = &drawings[&glyph.key];
        let Some(bounds) = drawing.bounds() else {
            continue;
        };
        let target = mapped_box(bounds, glyph.origin, glyph.scale, affine);
        if target.right <= target.left || target.bottom <= target.top {
            continue;
        }
        color |= !matches!(drawing, Drawing::Outline(_));
        union = Some(union.map_or(target, |held| PixelBox {
            left: held.left.min(target.left),
            top: held.top.min(target.top),
            right: held.right.max(target.right),
            bottom: held.bottom.max(target.bottom),
        }));
    }
    (union, color)
}

/// A glyph with its origin before the cluster's map and its pixels per font unit.
#[derive(Clone, Debug)]
struct Placed {
    key: DrawingKey,
    origin: [f32; 2],
    scale: f32,
}

/// A cluster's tile and stroke bands, in a frame offset by whole pixels when placed.
struct DrawnCluster {
    tile: Tile,
    padded: Tile,
    bands: Vec<Vec<u8>>,
}

impl Tile {
    fn moved(&self, [x, y]: [i64; 2]) -> Self {
        Self {
            left: self.left + x,
            top: self.top + y,
            width: self.width,
            height: self.height,
            coverage: Vec::new(),
            colors: None,
        }
    }
}

impl PixelBox {
    fn moved(self, [x, y]: [i64; 2]) -> Self {
        Self {
            left: self.left.saturating_add(x),
            top: self.top.saturating_add(y),
            right: self.right.saturating_add(x),
            bottom: self.bottom.saturating_add(y),
        }
    }
}

/// LRU cache of drawn clusters, so held or settled characters are drawn once. Bounded by
/// [`MAX_TILE_CACHE_BYTES`] and [`MAX_CACHED_CLUSTERS`].
#[derive(Debug, Default)]
pub(crate) struct TileCache {
    entries: HashMap<u64, (Arc<DrawnCluster>, u64)>,
    bytes: usize,
    next_use: u64,
}

impl std::fmt::Debug for DrawnCluster {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DrawnCluster")
            .field("width", &self.tile.width)
            .field("height", &self.tile.height)
            .finish_non_exhaustive()
    }
}

impl DrawnCluster {
    fn bytes(&self) -> usize {
        self.tile.coverage.len()
            + self.tile.colors.as_ref().map_or(0, Vec::len)
            + self.bands.iter().map(Vec::len).sum::<usize>()
    }
}

const MAX_CACHED_CLUSTERS: usize = 4_096;

impl TileCache {
    fn contains(&self, key: u64) -> bool {
        self.entries.contains_key(&key)
    }

    fn get(&mut self, key: u64) -> Option<Arc<DrawnCluster>> {
        self.next_use += 1;
        let use_now = self.next_use;
        self.entries.get_mut(&key).map(|(drawn, used)| {
            *used = use_now;
            Arc::clone(drawn)
        })
    }

    /// Keeps `drawn` unless it is larger than a quarter of the byte budget, evicting the least
    /// recently used entries to make room.
    fn insert(&mut self, key: u64, drawn: &Arc<DrawnCluster>) {
        let bytes = drawn.bytes();
        if bytes > MAX_TILE_CACHE_BYTES / 4 {
            return;
        }
        if self.entries.len() >= MAX_CACHED_CLUSTERS {
            // Drop the older half so eviction stays rare.
            let mut uses = self
                .entries
                .values()
                .map(|(_, used)| *used)
                .collect::<Vec<_>>();
            uses.sort_unstable();
            let cut = uses[uses.len() / 2];
            self.entries.retain(|_, (_, used)| *used > cut);
            self.bytes = self.entries.values().map(|(drawn, _)| drawn.bytes()).sum();
        }
        if self.bytes + bytes > MAX_TILE_CACHE_BYTES {
            let mut by_use = self
                .entries
                .iter()
                .map(|(key, (drawn, used))| (*used, *key, drawn.bytes()))
                .collect::<Vec<_>>();
            by_use.sort_unstable();
            for (_, old, old_bytes) in by_use {
                if self.bytes + bytes <= MAX_TILE_CACHE_BYTES {
                    break;
                }
                self.entries.remove(&old);
                self.bytes -= old_bytes;
            }
        }
        if let Some((replaced, _)) = self.entries.insert(key, (Arc::clone(drawn), self.next_use)) {
            self.bytes -= replaced.bytes();
        }
        self.bytes += bytes;
    }

    /// Bytes the cached tiles hold.
    #[cfg(test)]
    pub(crate) const fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

/// FNV-1a over the request fields that affect cluster drawing besides glyphs and maps.
fn look_key(request: &TextRasterRequest, strokes: &[TextStroke]) -> u64 {
    let mut hash = Fnv::default();
    hash.feed(&request.font_weight.to_le_bytes());
    hash.feed(&[request.emoji_policy as u8]);
    hash.feed(&request.fill.foreground());
    for stroke in strokes {
        hash.feed(&stroke.width.to_bits().to_le_bytes());
    }
    hash.0
}

fn cluster_key(look: u64, faces: &Faces<'_>, glyphs: &[Placed], affine: Affine) -> u64 {
    let mut hash = Fnv(look);
    let first = glyphs.first().map_or([0.0; 2], |glyph| glyph.origin);
    for glyph in glyphs {
        let (font, weight, italic) = faces.of[glyph.key.face];
        hash.feed(&font.to_le_bytes());
        hash.feed(&weight.to_le_bytes());
        hash.feed(&[u8::from(italic), u8::from(glyph.key.shear)]);
        hash.feed(&glyph.key.size.to_le_bytes());
        hash.feed(&glyph.key.foreground);
        hash.feed(&glyph.key.glyph.to_le_bytes());
        hash.feed(&glyph.scale.to_bits().to_le_bytes());
        for value in [glyph.origin[0] - first[0], glyph.origin[1] - first[1]] {
            hash.feed(&value.to_bits().to_le_bytes());
        }
    }
    // Key the map relative to the first glyph, so a moved cluster hits the cache.
    let [x, y] = apply(affine, first);
    for value in [affine[0], affine[1], affine[2], affine[3], x, y] {
        hash.feed(&value.to_bits().to_le_bytes());
    }
    hash.0
}

struct Fnv(u64);

impl Default for Fnv {
    fn default() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }
}

impl Fnv {
    fn feed(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn draw_cluster(
    strokes: &[TextStroke],
    drawings: &HashMap<DrawingKey, Drawing>,
    glyphs: &[Placed],
    affine: Affine,
) -> DrawnCluster {
    let mut tile = Tile::empty();
    for glyph in glyphs {
        let drawing = &drawings[&glyph.key];
        let Some(bounds) = drawing.bounds() else {
            continue;
        };
        let (origin, scale) = (glyph.origin, glyph.scale);
        let target = mapped_box(bounds, origin, scale, affine);
        if target.right <= target.left || target.bottom <= target.top {
            continue;
        }
        tile.grow(target);
        let width = usize::try_from(target.right - target.left).unwrap_or(0);
        match drawing {
            Drawing::Outline(outline) => {
                let height = usize::try_from(target.bottom - target.top).unwrap_or(0);
                let (left, top) = (target.left as f32, target.top as f32);
                let raster = outline.raster(width, height, |[x, y]| {
                    let [x, y] = apply(affine, [origin[0] + x * scale, origin[1] - y * scale]);
                    point(x - left, y - top)
                });
                raster.for_each_pixel(|at, coverage| {
                    let value = (coverage.min(1.0) * 255.0).round() as u8;
                    let x = target.left + (at % width) as i64;
                    let y = target.top + (at / width) as i64;
                    if let Some(slot) = tile.index(x, y) {
                        tile.coverage[slot] = tile.coverage[slot].max(value);
                    }
                });
            }
            drawing => {
                let Some(inverse) = invert(affine) else {
                    continue;
                };
                let upright = PixelBox::of(bounds, origin, scale);
                let rgba8 = resample(
                    &drawing.draw_color(origin, scale, upright),
                    upright,
                    target,
                    inverse,
                );
                let size = tile.width * tile.height * 4;
                let slots = (0..rgba8.len() / 4)
                    .map(|at| {
                        tile.index(
                            target.left + (at % width) as i64,
                            target.top + (at / width) as i64,
                        )
                    })
                    .collect::<Vec<_>>();
                let colors = tile.colors.get_or_insert_with(|| vec![0; size]);
                for (source, slot) in rgba8.as_chunks::<4>().0.iter().zip(slots) {
                    let Some(slot) = slot.filter(|_| source[3] > 0) else {
                        continue;
                    };
                    let held = &mut colors[slot * 4..slot * 4 + 4];
                    let blended = over(
                        source.map(u32::from),
                        std::array::from_fn(|channel| u32::from(held[channel])),
                    );
                    for (byte, value) in held.iter_mut().zip(blended) {
                        *byte = u8::try_from(value.min(255)).unwrap_or(u8::MAX);
                    }
                }
            }
        }
    }
    with_bands(tile, strokes)
}

#[expect(clippy::cast_possible_wrap, reason = "tile reach is a few pixels")]
fn with_bands(tile: Tile, strokes: &[TextStroke]) -> DrawnCluster {
    let reach = stroke_radius(strokes).map_or(0, disc_extent) as usize;
    let padded = Tile {
        left: tile.left - reach as i64,
        top: tile.top - reach as i64,
        width: tile.width + 2 * reach,
        height: tile.height + 2 * reach,
        coverage: Vec::new(),
        colors: None,
    };
    let mut bands = Vec::with_capacity(strokes.len());
    if !strokes.is_empty() && tile.width > 0 && tile.height > 0 {
        let mut ink = vec![0_u8; padded.width * padded.height];
        for row in 0..tile.height {
            for column in 0..tile.width {
                let at = row * tile.width + column;
                let alpha = tile.colors.as_ref().map_or(0, |held| held[at * 4 + 3]);
                ink[(row + reach) * padded.width + column + reach] = tile.coverage[at].max(alpha);
            }
        }
        let mut radius = 0.0;
        for stroke in strokes {
            radius += stroke.width;
            bands.push(dilate(
                &ink,
                u32::try_from(padded.width).unwrap_or(u32::MAX),
                u32::try_from(padded.height).unwrap_or(u32::MAX),
                radius,
            ));
        }
    }
    DrawnCluster {
        tile,
        padded,
        bands,
    }
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "pixel boxes of layout positions"
)]
fn draw_box(rect: [f32; 4], affine: Affine, strokes: &[TextStroke]) -> DrawnCluster {
    let [left, top, right, bottom] = rect;
    let corners = [[left, top], [right, top], [right, bottom], [left, bottom]]
        .map(|corner| apply(affine, corner));
    let (mut x_min, mut y_min, mut x_max, mut y_max) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for [x, y] in corners {
        (x_min, y_min, x_max, y_max) = (x_min.min(x), y_min.min(y), x_max.max(x), y_max.max(y));
    }
    let bounds = PixelBox {
        left: x_min.floor() as i64,
        top: y_min.floor() as i64,
        right: x_max.ceil() as i64,
        bottom: y_max.ceil() as i64,
    };
    let mut tile = Tile::empty();
    if bounds.right > bounds.left && bounds.bottom > bounds.top {
        tile.grow(bounds);
        let (origin_x, origin_y) = (bounds.left as f32, bounds.top as f32);
        let mut raster = ab_glyph_rasterizer::Rasterizer::new(tile.width, tile.height);
        for (at, corner) in corners.iter().enumerate() {
            let next = corners[(at + 1) % corners.len()];
            raster.draw_line(
                point(corner[0] - origin_x, corner[1] - origin_y),
                point(next[0] - origin_x, next[1] - origin_y),
            );
        }
        raster.for_each_pixel(|at, coverage| {
            tile.coverage[at] = (coverage.min(1.0) * 255.0).round() as u8;
        });
    }
    with_bands(tile, strokes)
}

/// Line start in the box, as [`Layout::line_offset`] computes it.
fn line_start(layout: &Layout, rtl: bool, width: f32, request: &TextRasterRequest) -> f32 {
    use alpha2_media::TextAlign;
    match (request.align, rtl) {
        (TextAlign::Center, _) => (layout.width - width) * 0.5,
        (TextAlign::Start, false) | (TextAlign::End, true) => 0.0,
        (TextAlign::End, false) | (TextAlign::Start, true) => layout.width - width,
    }
}

fn text_layout(
    layout: &Layout,
    shaping: &super::runs::Shaping,
    request: &TextRasterRequest,
) -> TextLayout {
    let mut map = Vec::new();
    normalize_mapped(&request.text, request.emoji_policy, Some(&mut map));
    let original = |at: usize| map.get(at).copied().unwrap_or(request.text.len());
    let words = request
        .text
        .split_word_bound_indices()
        .filter(|(_, word)| word.chars().any(char::is_alphanumeric))
        .map(|(start, word)| start..start + word.len())
        .collect::<Vec<_>>();
    let mut clusters = layout
        .clusters
        .iter()
        .map(|span| {
            let bytes = original(span.bytes.start)..original(span.bytes.end);
            let text = &request.text[bytes.clone()];
            let word = words
                .partition_point(|word| word.end <= bytes.start)
                .try_into()
                .ok()
                .filter(|&index: &u32| {
                    !span.ellipsis
                        && words
                            .get(index as usize)
                            .is_some_and(|word| word.contains(&bytes.start))
                });
            ClusterInfo {
                is_space: !text.is_empty() && text.chars().all(char::is_whitespace),
                bytes,
                line: u32::try_from(span.line).unwrap_or(u32::MAX),
                word,
                x: 0.0,
                advance: 0.0,
                baseline: 0.0,
                font_size: shaping.looks[usize::from(span.look)].size,
                ink: None,
                ellipsis: span.ellipsis,
            }
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::with_capacity(layout.lines.len());
    let mut first_cluster = 0;
    for (index, line) in layout.lines.iter().enumerate() {
        let left = layout.line_offset(line, request.align);
        let baseline = layout.baseline(index);
        let line_number = u32::try_from(index).unwrap_or(u32::MAX);
        let last_cluster = clusters[first_cluster..]
            .iter()
            .position(|cluster| cluster.line != line_number)
            .map_or(clusters.len(), |count| first_cluster + count);
        let mut extents = HashMap::<usize, (f32, f32)>::new();
        for glyph in &line.glyphs {
            let start = glyph.x + left;
            let extent = extents.entry(glyph.cluster).or_insert((start, start));
            *extent = (extent.0.min(start), extent.1.max(start + glyph.advance));
        }
        for (cluster, info) in clusters[first_cluster..last_cluster].iter_mut().enumerate() {
            let (start, end) = extents
                .get(&(first_cluster + cluster))
                .copied()
                .unwrap_or((left + line.width, left + line.width));
            info.x = start;
            info.advance = end - start;
            info.baseline = baseline;
        }
        let size = layout.line_size(index);
        lines.push(LineInfo {
            clusters: first_cluster..last_cluster,
            left,
            width: line.width,
            baseline,
            size,
            box_top: layout.line_top(index) - super::half_leading(size, request.line_height),
        });
        first_cluster = last_cluster;
    }
    TextLayout {
        clusters,
        lines,
        font_size: request.font_size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(bytes: usize) -> Arc<DrawnCluster> {
        Arc::new(DrawnCluster {
            tile: Tile {
                left: 0,
                top: 0,
                width: bytes,
                height: 1,
                coverage: vec![0; bytes],
                colors: None,
            },
            padded: Tile::empty(),
            bands: Vec::new(),
        })
    }

    #[test]
    fn the_tile_cache_evicts_the_least_recently_used_by_bytes() {
        let mut cache = TileCache::default();
        let tile = 10 << 20;
        for key in 0..6 {
            cache.insert(key, &drawn(tile));
        }
        assert_eq!(cache.bytes(), 6 * tile);
        assert!(cache.get(0).is_some());
        cache.insert(6, &drawn(tile));
        assert!(cache.bytes() <= MAX_TILE_CACHE_BYTES);
        assert!(cache.contains(0) && cache.contains(6) && !cache.contains(1));
        cache.insert(7, &drawn(MAX_TILE_CACHE_BYTES / 4 + 1));
        assert!(!cache.contains(7));
        cache.insert(6, &drawn(tile));
        assert!(cache.bytes() <= MAX_TILE_CACHE_BYTES);
        cache.clear();
        assert_eq!(cache.bytes(), 0);
    }
}
