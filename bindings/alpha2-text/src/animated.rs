//! Text with a state for each of its clusters: per-character motion and
//! color, as text animators give it.
//!
//! [`TextRasterizer::layout`] answers where each cluster lies, so a caller
//! can compute the states; [`TextRasterizer::rasterize_animated`] draws the
//! text with them. Lines are those of the text without states: tracking
//! moves clusters along their line and re-aligns it, never re-wraps.
//!
//! Each cluster's glyph outlines are transformed before they are rasterized
//! (an affine map keeps lines and curves as lines and curves), so a scaled or
//! turned character is as sharp as an upright one. Each cluster is drawn into
//! a tile of its own; tiles are combined by the larger coverage, the color
//! coming from the cluster that gives it. Strokes dilate each cluster's tile
//! and combine the same way. Color glyphs are drawn upright and resampled
//! bilinearly in fixed point. All of it is integer or `libm` arithmetic, so
//! native and wasm draw the same bytes.

use super::{
    Drawing, Faces, HashMap, HostTexture, HostTexturePixels, Layout, PixelBox, PixelLook,
    Premultiplication, RasterColorSpace, RequestLook, TextFill, TextRasterError, TextRasterRequest,
    TextRasterResult, TextRasterizer, TextTexture, decoration_extents, dilate, disc_extent,
    gradient, normalize_mapped, over, point, stroke_radius, validate_request,
};
use std::ops::Range;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

/// Where a request's text lies: its clusters and lines, laid out as
/// [`TextRasterizer::rasterize`] lays them out.
#[derive(Clone, Debug, PartialEq)]
pub struct TextLayout {
    /// Every cluster (the unit letter spacing counts in), in logical order.
    pub clusters: Vec<ClusterInfo>,
    /// Every laid-out line, top to bottom.
    pub lines: Vec<LineInfo>,
    /// The font size in pixels.
    pub font_size: f32,
}

/// One cluster of a [`TextLayout`].
#[derive(Clone, Debug, PartialEq)]
pub struct ClusterInfo {
    /// Its bytes in the request's text.
    pub bytes: Range<usize>,
    /// Its line.
    pub line: u32,
    /// The UAX #29 word it belongs to, counting only words that hold a
    /// letter or a digit; `None` for a cluster in no such word (a space or
    /// punctuation between words).
    pub word: Option<u32>,
    /// All it holds is white space.
    pub is_space: bool,
    /// Where its advance starts on its line, in pixels from the layout box's
    /// left edge, in visual order.
    pub x: f32,
    /// Its advance in pixels; zero for a space that hangs at a line's end.
    pub advance: f32,
    /// Its line's baseline, in pixels from the layout box's top.
    pub baseline: f32,
}

/// One line of a [`TextLayout`].
#[derive(Clone, Debug, PartialEq)]
pub struct LineInfo {
    /// Its clusters, in logical order.
    pub clusters: Range<usize>,
    /// Where it starts, in pixels from the layout box's left edge.
    pub left: f32,
    /// Its advance, trailing spaces left out.
    pub width: f32,
    /// Its baseline, in pixels from the layout box's top.
    pub baseline: f32,
}

/// How much of a color a cluster keeps and what it gains: the color becomes
/// `base * keep + add`, in premultiplied linear light. Mixing toward a target
/// by `w` is `keep = 1 - w`, `add = w * target`; several mixes compose.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ColorMix {
    /// What is kept of the base color, in `[0, 1]`.
    pub keep: f32,
    /// What is added, premultiplied linear-light RGBA in `[0, 1]`.
    pub add: [f32; 4],
}

/// What a cluster's animators do to it.
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct ClusterState {
    /// Moved by this, in pixels, +y down.
    pub offset: [f32; 2],
    /// Scaled about its centre by this on each axis, at least zero.
    pub scale: [f32; 2],
    /// Turned about its centre by this, clockwise radians.
    pub rotation: f32,
    /// Its coverage and its strokes' scaled by this, in `[0, 1]`.
    pub opacity: f32,
    /// How its fill color changes; `None` keeps it.
    pub fill: Option<ColorMix>,
    /// How every stroke's color changes; `None` keeps them.
    pub stroke: Option<ColorMix>,
    /// Space added after it, in pixels, as letter spacing adds it.
    pub tracking: f32,
}

impl ClusterState {
    /// A cluster as the text without states draws it.
    pub const IDENTITY: Self = Self {
        offset: [0.0, 0.0],
        scale: [1.0, 1.0],
        rotation: 0.0,
        opacity: 1.0,
        fill: None,
        stroke: None,
        tracking: 0.0,
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
                .all(|value| value.is_finite() && *value >= 0.0)
            && self.rotation.is_finite()
            && (0.0..=1.0).contains(&self.opacity)
            && self.tracking.is_finite()
            && mix(&self.fill)
            && mix(&self.stroke)
    }
}

/// An affine map of layout pixels: `[a, b, c, d, e, f]` sends `(x, y)` to
/// `(a x + c y + e, b x + d y + f)`.
type Affine = [f32; 6];

fn apply(affine: Affine, [x, y]: [f32; 2]) -> [f32; 2] {
    [
        affine[0] * x + affine[2] * y + affine[4],
        affine[1] * x + affine[3] * y + affine[5],
    ]
}

/// The map that scales and turns about `pivot`, then moves by `offset`.
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

/// The inverse of `affine`, when it has one.
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

/// A rectangle of pixels in the layout box (which may lie past it), right
/// and bottom exclusive, holding one cluster's coverage and color glyphs.
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

    /// The tile grown to hold `bounds`, its pixels kept.
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

/// The pixels the corners of a glyph's font-unit `bounds` reach once placed
/// at `origin` at `scale` and mapped by `affine`.
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

/// A color glyph's upright premultiplied RGBA over `source` resampled into
/// `target` by `affine`: each target pixel's centre mapped back, sampled
/// bilinearly with 8-bit fractional weights.
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

/// `color` changed by `mix`, as straight sRGB bytes.
fn mixed(color: [u8; 4], mix: &ColorMix) -> [u8; 4] {
    let base = gradient::premultiplied_linear(color.map(|value| f32::from(value) / 255.0));
    gradient::stored_rgba8(std::array::from_fn(|channel| {
        base[channel] * mix.keep + mix.add[channel]
    }))
}

/// The look of an animated raster: each pixel's fill and stroke colors from
/// the cluster that gives it its coverage.
struct ClusterLook<'a> {
    base: RequestLook<'a>,
    states: &'a [ClusterState],
    /// The cluster that gives each pixel its fill coverage, `u32::MAX` for none.
    fill_owner: Vec<u32>,
    /// The same for each stroke's band.
    band_owner: Vec<Vec<u32>>,
    /// The cluster whose color glyph was drawn last at each pixel.
    color_owner: Vec<u32>,
    /// Each cluster's solid fill, mixed once.
    solid: Vec<Option<[u8; 4]>>,
    /// Each stroke's color for each cluster, mixed once.
    strokes: Vec<Vec<[u8; 4]>>,
}

impl PixelLook for ClusterLook<'_> {
    fn fill(&self, index: usize, point: [f32; 2]) -> [u8; 4] {
        let owner = self.fill_owner[index] as usize;
        let Some(state) = self.states.get(owner) else {
            return self.base.fill(index, point);
        };
        match (&self.base.gradient, state.fill) {
            (Some(gradient), Some(mix)) => {
                let base = gradient.linear_at(point);
                gradient::stored_rgba8(std::array::from_fn(|channel| {
                    base[channel] * mix.keep + mix.add[channel]
                }))
            }
            (None, Some(_)) => self.solid[owner].unwrap_or(self.base.foreground),
            (_, None) => self.base.fill(index, point),
        }
    }

    fn band(&self, stroke: usize, index: usize) -> [u8; 4] {
        let owner = self.band_owner[stroke][index] as usize;
        self.strokes[stroke]
            .get(owner)
            .copied()
            .unwrap_or_else(|| self.base.band(stroke, index))
    }

    /// The opacity of the cluster that owns the pixel: its fill's, else its
    /// color glyph's, else its innermost band's.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn fade(&self, index: usize) -> Option<u32> {
        let owner = [self.fill_owner[index], self.color_owner[index]]
            .into_iter()
            .chain(self.band_owner.iter().map(|owners| owners[index]))
            .find(|&owner| owner != u32::MAX)?;
        let opacity = (self.states.get(owner as usize)?.opacity * 255.0).round() as u32;
        (opacity < 255).then_some(opacity)
    }
}

/// Puts `tile`'s coverage at `opacity` into `plane` (`width` wide, its
/// layout box corner at `margin`) where it is the larger, ties going to the
/// later cluster, and records `cluster` as the owner.
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
    /// Where `request`'s text lies: every cluster and line, laid out as
    /// [`Self::rasterize`] lays them out. The request's look is ignored.
    ///
    /// # Errors
    /// Returns the validation, font, or size error [`Self::rasterize`]
    /// returns for the same request.
    pub fn layout(&self, request: &TextRasterRequest) -> Result<TextLayout, TextRasterError> {
        validate_request(request)?;
        if self.fonts.is_empty() {
            return Err(TextRasterError::NoFonts);
        }
        let faces = Faces::new(&self.fonts, request.font_weight)?;
        let layout = self.lay_out(&faces, request)?;
        Ok(text_layout(&layout, request))
    }

    /// [`Self::rasterize`] with each cluster of [`Self::layout`] changed by
    /// its state in `states`, in logical order. All states identity draw
    /// exactly what [`Self::rasterize`] draws. The result is cached by the
    /// request's content version as [`Self::rasterize`]'s is: give each set
    /// of states a version of its own.
    ///
    /// # Errors
    /// Returns [`TextRasterError::InvalidRequest`] for states that are not
    /// one a cluster or not in range, and otherwise what [`Self::rasterize`]
    /// returns.
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
    fn rasterize_states(
        &self,
        request: &TextRasterRequest,
        states: &[ClusterState],
        cache: &mut TileCache,
    ) -> Result<TextRasterResult, TextRasterError> {
        let faces = Faces::new(&self.fonts, request.font_weight)?;
        let layout = self.lay_out(&faces, request)?;
        if states.len() != layout.clusters.len() {
            return Err(TextRasterError::InvalidRequest);
        }
        let (box_width, box_height) = (layout.box_width, layout.box_height);
        let ascent = layout.ascent;
        let pivot_drop = request.font_size * 0.3;
        // Each cluster's glyphs, placed, and the map its state gives it.
        let mut placed = vec![(Vec::<Placed>::new(), [0.0_f32; 6]); states.len()];
        let mut drawings = HashMap::<(usize, u16), Drawing>::new();
        let (mut ink_ascent, mut ink_descent) = (layout.ascent, layout.descent);
        for (index, line) in layout.lines.iter().enumerate() {
            let baseline = layout.line_top(index) + ascent;
            // Tracking in visual order: after each cluster but the line's last.
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
            let width = line.width + shift;
            let offset = line_start(&layout, line.rtl, width, request);
            // Each cluster's pivot: the middle of its advance, the middle of its em box.
            let mut extents = HashMap::<usize, (f32, f32)>::new();
            for (glyph, shift) in line.glyphs.iter().zip(&shifts) {
                let left = glyph.x + shift + offset;
                let extent = extents.entry(glyph.cluster).or_insert((left, left));
                *extent = (extent.0.min(left), extent.1.max(left + glyph.advance));
            }
            for (glyph, shift) in line.glyphs.iter().zip(&shifts) {
                let (left, right) = extents[&glyph.cluster];
                let affine = cluster_affine(
                    &states[glyph.cluster],
                    [f32::midpoint(left, right), baseline - pivot_drop],
                );
                let drawing = drawings
                    .entry((glyph.font, glyph.glyph_index))
                    .or_insert_with(|| Drawing::of(&faces, glyph.font, glyph.glyph_index, request));
                let Some(bounds) = drawing.bounds() else {
                    continue;
                };
                let scale = request.font_size / faces.units_per_em(glyph.font);
                ink_ascent = ink_ascent.max(bounds[3] * scale + glyph.y_offset);
                ink_descent = ink_descent.max(-(bounds[1] * scale + glyph.y_offset));
                let slot = &mut placed[glyph.cluster];
                slot.1 = affine;
                slot.0.push(Placed {
                    key: (glyph.font, glyph.glyph_index),
                    origin: [
                        glyph.x + shift + glyph.x_offset + offset,
                        baseline - glyph.y_offset,
                    ],
                    scale,
                });
            }
        }
        // Each cluster's tiles, drawn or found again: they depend only on its glyphs, the linear
        // part of its map and where its first glyph lands within a pixel.
        let look_key = look_key(request);
        let mut tiles = Vec::with_capacity(states.len());
        for (glyphs, affine) in &placed {
            let Some(first) = glyphs.first() else {
                tiles.push(None);
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
            let key = cluster_key(look_key, glyphs, local);
            let drawn = if let Some(drawn) = cache.get(key) {
                drawn
            } else {
                let drawn = Arc::new(draw_cluster(request, &drawings, glyphs, local));
                cache.insert(key, &drawn);
                drawn
            };
            tiles.push(Some((drawn, [whole[0] as i64, whole[1] as i64])));
        }
        // The texture is the box plus a margin, as the static raster's: equal on both ends of an
        // axis, with room for the strokes around the ink.
        let (outline, padding) = decoration_extents(request);
        let (mut overflow_x, mut overflow_y) = (0_i64, 0_i64);
        for bounds in tiles
            .iter()
            .flatten()
            .filter_map(|(drawn, at)| drawn.tile.bounds().map(|bounds| bounds.moved(*at)))
        {
            overflow_x = overflow_x
                .max(bounds.left.saturating_neg())
                .max(bounds.right.saturating_sub(i64::from(box_width)));
            overflow_y = overflow_y
                .max(bounds.top.saturating_neg())
                .max(bounds.bottom.saturating_sub(i64::from(box_height)));
        }
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
        let pixel_count = width as usize * height as usize;
        let margins = [margin_x, margin_y];
        let mut coverage = vec![0_u8; pixel_count];
        let mut fill_owner = vec![u32::MAX; pixel_count];
        let mut color_owner = vec![u32::MAX; pixel_count];
        let mut colors: Option<Vec<u8>> = None;
        let mut bands = vec![vec![0_u8; pixel_count]; request.strokes.len()];
        let mut band_owner = vec![vec![u32::MAX; pixel_count]; request.strokes.len()];
        for (cluster, (drawn, at)) in tiles
            .iter()
            .enumerate()
            .filter_map(|(cluster, tile)| tile.as_ref().map(|tile| (cluster, tile)))
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
        let base = RequestLook::of(request, [layout.width, layout.height]);
        let solid = match &request.fill {
            TextFill::Solid(color) => states
                .iter()
                .map(|state| state.fill.map(|mix| mixed(*color, &mix)))
                .collect(),
            TextFill::Gradient(_) => vec![None; states.len()],
        };
        let strokes = request
            .strokes
            .iter()
            .map(|stroke| {
                states
                    .iter()
                    .map(|state| {
                        state
                            .stroke
                            .map_or(stroke.color, |mix| mixed(stroke.color, &mix))
                    })
                    .collect()
            })
            .collect();
        let look = ClusterLook {
            base,
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
                first_baseline: layout.half_leading + layout.ascent,
                ascent: ink_ascent,
                descent: ink_descent,
                line_count: layout.line_count,
            },
            pixels: HostTexturePixels { texture, rgba8 },
            background,
        })
    }
}

/// One glyph of a cluster, placed: its drawing, its origin in the layout
/// box before the cluster's map, and its pixels a font unit.
#[derive(Clone, Debug)]
struct Placed {
    key: (usize, u16),
    origin: [f32; 2],
    scale: f32,
}

/// A cluster drawn by itself: its coverage and color glyphs, and each
/// stroke's band around them in `padded`, in a frame whose whole-pixel
/// position is added when it is placed.
struct DrawnCluster {
    tile: Tile,
    padded: Tile,
    bands: Vec<Vec<u8>>,
}

impl Tile {
    /// The tile's geometry moved by whole pixels, without its pixels.
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
            left: self.left + x,
            top: self.top + y,
            right: self.right + x,
            bottom: self.bottom + y,
        }
    }
}

/// Drawn clusters by what draws them, the least recently used out past a
/// count: a held caption's characters, and those an animation has settled,
/// are drawn once.
#[derive(Debug, Default)]
pub(crate) struct TileCache {
    entries: HashMap<u64, (Arc<DrawnCluster>, u64)>,
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

/// Most drawn clusters the cache keeps: a few long captions' worth.
const MAX_CACHED_CLUSTERS: usize = 4_096;

impl TileCache {
    fn get(&mut self, key: u64) -> Option<Arc<DrawnCluster>> {
        self.next_use += 1;
        let use_now = self.next_use;
        self.entries.get_mut(&key).map(|(drawn, used)| {
            *used = use_now;
            Arc::clone(drawn)
        })
    }

    fn insert(&mut self, key: u64, drawn: &Arc<DrawnCluster>) {
        if self.entries.len() >= MAX_CACHED_CLUSTERS {
            // Drop the older half at once, so eviction stays rare.
            let mut uses = self
                .entries
                .values()
                .map(|(_, used)| *used)
                .collect::<Vec<_>>();
            uses.sort_unstable();
            let cut = uses[uses.len() / 2];
            self.entries.retain(|_, (_, used)| *used > cut);
        }
        self.entries.insert(key, (Arc::clone(drawn), self.next_use));
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
    }
}

/// FNV-1a over what a request's clusters are drawn with, besides their
/// glyphs and maps: the weight, the emoji policy, the color a color glyph's
/// text parts take, and the strokes' widths.
fn look_key(request: &TextRasterRequest) -> u64 {
    let mut hash = Fnv::default();
    hash.feed(&request.font_weight.to_le_bytes());
    hash.feed(&[request.emoji_policy as u8]);
    hash.feed(&request.fill.foreground());
    for stroke in &request.strokes {
        hash.feed(&stroke.width.to_bits().to_le_bytes());
    }
    hash.0
}

/// A cluster's draw key: the look, then each glyph and its origin from the
/// first's, then the map.
fn cluster_key(look: u64, glyphs: &[Placed], affine: Affine) -> u64 {
    let mut hash = Fnv(look);
    let first = glyphs.first().map_or([0.0; 2], |glyph| glyph.origin);
    for glyph in glyphs {
        hash.feed(&glyph.key.0.to_le_bytes());
        hash.feed(&glyph.key.1.to_le_bytes());
        hash.feed(&glyph.scale.to_bits().to_le_bytes());
        for value in [glyph.origin[0] - first[0], glyph.origin[1] - first[1]] {
            hash.feed(&value.to_bits().to_le_bytes());
        }
    }
    // The map as it moves the first glyph: the same relative to it, the same cluster.
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

/// One cluster's glyphs mapped by `affine` and drawn into a tile of their
/// own, and its strokes' bands around them.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn draw_cluster(
    request: &TextRasterRequest,
    drawings: &HashMap<(usize, u16), Drawing>,
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
    // The cluster's ink, its color glyphs' alpha too, dilated band by band.
    let reach = stroke_radius(&request.strokes).map_or(0, disc_extent) as usize;
    let padded = Tile {
        left: tile.left - reach as i64,
        top: tile.top - reach as i64,
        width: tile.width + 2 * reach,
        height: tile.height + 2 * reach,
        coverage: Vec::new(),
        colors: None,
    };
    let mut bands = Vec::with_capacity(request.strokes.len());
    if !request.strokes.is_empty() && tile.width > 0 && tile.height > 0 {
        let mut ink = vec![0_u8; padded.width * padded.height];
        for row in 0..tile.height {
            for column in 0..tile.width {
                let at = row * tile.width + column;
                let alpha = tile.colors.as_ref().map_or(0, |held| held[at * 4 + 3]);
                ink[(row + reach) * padded.width + column + reach] = tile.coverage[at].max(alpha);
            }
        }
        let mut radius = 0.0;
        for stroke in &request.strokes {
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

/// Where a line of `width` starts in the box, as [`Layout::line_offset`]
/// places one of its own width.
fn line_start(layout: &Layout, rtl: bool, width: f32, request: &TextRasterRequest) -> f32 {
    use alpha2_media::TextAlign;
    match (request.align, rtl) {
        (TextAlign::Center, _) => (layout.width - width) * 0.5,
        (TextAlign::Start, false) | (TextAlign::End, true) => 0.0,
        (TextAlign::End, false) | (TextAlign::Start, true) => layout.width - width,
    }
}

/// The public layout of `layout`, its bytes those of `request`'s text.
fn text_layout(layout: &Layout, request: &TextRasterRequest) -> TextLayout {
    let mut map = Vec::new();
    normalize_mapped(&request.text, request.emoji_policy, Some(&mut map));
    let original = |at: usize| map.get(at).copied().unwrap_or(request.text.len());
    // Words that hold a letter or digit, by their first byte.
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
                    words
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
            }
        })
        .collect::<Vec<_>>();
    let mut lines = Vec::with_capacity(layout.lines.len());
    let mut first_cluster = 0;
    for (index, line) in layout.lines.iter().enumerate() {
        let left = layout.line_offset(line, request.align);
        let baseline = layout.line_top(index) + layout.ascent;
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
        lines.push(LineInfo {
            clusters: first_cluster..last_cluster,
            left,
            width: line.width,
            baseline,
        });
        first_cluster = last_cluster;
    }
    TextLayout {
        clusters,
        lines,
        font_size: request.font_size,
    }
}
