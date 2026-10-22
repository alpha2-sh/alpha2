//! Color glyphs: COLR paintings (versions 0 and 1) and CBDT or sbix
//! bitmaps, drawn to premultiplied RGBA.
//!
//! Everything is drawn here, in `f32` arithmetic that rounds the same on
//! every target (no fused operations, `libm` for the one transcendental
//! function), so a color glyph is the same bytes natively and in wasm, as
//! an outline glyph is. The text's color reaches a painting only where the
//! font asks for it (palette index 0xFFFF); its alpha applies to the whole
//! glyph when the texture is composed.

use std::io::Cursor;

use ab_glyph_rasterizer::point;
use rustybuzz::ttf_parser::colr::{ClipBox, CompositeMode, GradientExtend, Paint, Painter};
use rustybuzz::ttf_parser::{self, GlyphId, RasterImageFormat, RgbaColor, Transform};

use super::{Outline, PixelBox, Segment};

/// Premultiplied RGBA, each in `[0, 1]`.
type Color = [f32; 4];

/// One COLR glyph as the table paints it, recorded once a request: what
/// each call fills and clips with, in font units, y up.
pub(super) struct Painting {
    outlines: Vec<Outline>,
    commands: Vec<Command>,
    /// `[x_min, y_min, x_max, y_max]` holding every outline and clip box
    /// it draws with, in font units; `None` when it draws with none.
    bounds: Option<[f32; 4]>,
}

/// An outline as one call places it.
#[derive(Copy, Clone)]
struct Shape {
    outline: usize,
    transform: Transform,
}

enum Command {
    /// Fills the clip, and `shape` within it when there is one, with
    /// `fill` in the space of `transform`.
    Fill {
        shape: Option<Shape>,
        transform: Transform,
        fill: Fill,
    },
    PushClip(Shape),
    PopClip,
    /// What is painted until the matching [`Command::PopLayer`] goes on a
    /// layer of its own, then onto the one below in this mode.
    PushLayer(CompositeMode),
    PopLayer,
}

enum Fill {
    Solid(Color),
    /// The color line runs from `start` to `end`, constant across it.
    Linear {
        start: [f32; 2],
        end: [f32; 2],
        line: ColorLine,
    },
    /// The color line runs over the circles from `(start, start_radius)` to
    /// `(end, end_radius)`.
    Radial {
        start: [f32; 2],
        start_radius: f32,
        end: [f32; 2],
        end_radius: f32,
        line: ColorLine,
    },
    /// The color line runs counter-clockwise around `center` from
    /// `start_angle` to `end_angle`, in degrees.
    Sweep {
        center: [f32; 2],
        start_angle: f32,
        end_angle: f32,
        line: ColorLine,
    },
}

/// A gradient's stops by offset, as the table lists those with equal
/// offsets, and what lies past the first and the last.
struct ColorLine {
    stops: Vec<(f32, Color)>,
    extend: GradientExtend,
}

impl Painting {
    /// The COLR painting of `glyph`, with `foreground` (straight RGBA) where
    /// the font names the text's color; `None` when the table has no
    /// painting for it or it draws with nothing.
    pub(super) fn of(face: &ttf_parser::Face<'_>, glyph: u16, foreground: [u8; 4]) -> Option<Self> {
        if !face.is_color_glyph(GlyphId(glyph)) {
            return None;
        }
        let mut recorder = Recorder {
            face,
            painting: Self {
                outlines: Vec::new(),
                commands: Vec::new(),
                bounds: None,
            },
            transform: Transform::default(),
            transforms: Vec::new(),
            pending: None,
        };
        let [red, green, blue, alpha] = foreground;
        face.paint_color_glyph(
            GlyphId(glyph),
            0,
            RgbaColor::new(red, green, blue, alpha),
            &mut recorder,
        )?;
        let painting = recorder.painting;
        painting.bounds.is_some().then_some(painting)
    }

    /// `[x_min, y_min, x_max, y_max]` in font units: all it can draw.
    pub(super) fn bounds(&self) -> Option<[f32; 4]> {
        self.bounds
    }

    /// The painting over `pixels`, premultiplied RGBA8 row by row, with its
    /// origin at `origin` at `scale`.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(super) fn draw(&self, origin: [f32; 2], scale: f32, pixels: PixelBox) -> Vec<u8> {
        let place = Place::of(origin, scale, pixels);
        let count = place.width * place.height;
        let mut layers = vec![(vec![[0.0_f32; 4]; count], CompositeMode::SourceOver)];
        // Each entry is every clip pushed so far, multiplied together.
        let mut clips = Vec::<Vec<f32>>::new();
        for command in &self.commands {
            match command {
                Command::Fill {
                    shape,
                    transform,
                    fill,
                } => {
                    let Some(inverse) = invert(*transform) else {
                        continue;
                    };
                    let within = shape.map(|shape| self.coverage(shape, &place));
                    let clip = clips.last();
                    let layer = &mut layers.last_mut().expect("the base layer stays").0;
                    for (index, pixel) in layer.iter_mut().enumerate() {
                        let mut covered = clip.map_or(1.0, |clip| clip[index]);
                        if let Some(within) = &within {
                            covered *= within[index];
                        }
                        if covered <= 0.0 {
                            continue;
                        }
                        let color = fill.at(apply(inverse, place.font_point(index)));
                        *pixel = composite(
                            CompositeMode::SourceOver,
                            color.map(|channel| channel * covered),
                            *pixel,
                        );
                    }
                }
                Command::PushClip(shape) => {
                    let mut coverage = self.coverage(*shape, &place);
                    if let Some(outer) = clips.last() {
                        for (value, outer) in coverage.iter_mut().zip(outer) {
                            *value *= outer;
                        }
                    }
                    clips.push(coverage);
                }
                Command::PopClip => {
                    clips.pop();
                }
                Command::PushLayer(mode) => layers.push((vec![[0.0; 4]; count], *mode)),
                Command::PopLayer => pop_layer(&mut layers, clips.last()),
            }
        }
        // A painting cut short leaves layers open; they close as their end would have.
        while layers.len() > 1 {
            pop_layer(&mut layers, None);
        }
        let mut rgba8 = vec![0_u8; count * 4];
        for (pixel, color) in rgba8.as_chunks_mut::<4>().0.iter_mut().zip(&layers[0].0) {
            for (byte, channel) in pixel.iter_mut().zip(color) {
                *byte = (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
            }
        }
        rgba8
    }

    /// The coverage of `shape` over the pixels of `place`, each in `[0, 1]`.
    fn coverage(&self, shape: Shape, place: &Place) -> Vec<f32> {
        let raster = self.outlines[shape.outline].raster(place.width, place.height, |at| {
            let [x, y] = place.pixel(apply(shape.transform, at));
            point(x, y)
        });
        let mut coverage = vec![0.0_f32; place.width * place.height];
        raster.for_each_pixel(|index, value| coverage[index] = value.min(1.0));
        coverage
    }
}

/// Composites the top layer onto the one below in its mode, within `clip`
/// (outside it the layer below stays as it was).
fn pop_layer(layers: &mut Vec<(Vec<Color>, CompositeMode)>, clip: Option<&Vec<f32>>) {
    if layers.len() < 2 {
        return;
    }
    let (source, mode) = layers.pop().expect("two layers");
    let backdrop = &mut layers.last_mut().expect("one layer").0;
    for (index, (pixel, source)) in backdrop.iter_mut().zip(source).enumerate() {
        let mixed = composite(mode, source, *pixel);
        let within = clip.map_or(1.0, |clip| clip[index]);
        *pixel = if within >= 1.0 {
            mixed
        } else {
            std::array::from_fn(|channel| {
                pixel[channel] + (mixed[channel] - pixel[channel]) * within
            })
        };
    }
}

/// Where a glyph's pixels lie: the pixel box of a glyph with its origin at
/// `origin` (layout pixels, y down) at `scale` pixels a font unit.
struct Place {
    origin: [f32; 2],
    scale: f32,
    left: f32,
    top: f32,
    width: usize,
    height: usize,
}

impl Place {
    #[allow(clippy::cast_precision_loss)]
    fn of(origin: [f32; 2], scale: f32, pixels: PixelBox) -> Self {
        Self {
            origin,
            scale,
            left: pixels.left as f32,
            top: pixels.top as f32,
            width: usize::try_from(pixels.right - pixels.left).unwrap_or(0),
            height: usize::try_from(pixels.bottom - pixels.top).unwrap_or(0),
        }
    }

    /// A point in font units in the box's pixels, by the expression
    /// `Outline::draw` maps a point by.
    fn pixel(&self, [x, y]: [f32; 2]) -> [f32; 2] {
        [
            self.origin[0] + x * self.scale - self.left,
            (self.origin[1] - y * self.scale) - self.top,
        ]
    }

    /// The centre of pixel `index` in font units.
    #[allow(clippy::cast_precision_loss)]
    fn font_point(&self, index: usize) -> [f32; 2] {
        let x = self.left + (index % self.width) as f32 + 0.5;
        let y = self.top + (index / self.width) as f32 + 0.5;
        [
            (x - self.origin[0]) / self.scale,
            (self.origin[1] - y) / self.scale,
        ]
    }
}

/// Records the calls of a COLR painting.
struct Recorder<'f, 'a> {
    face: &'f ttf_parser::Face<'a>,
    painting: Painting,
    transform: Transform,
    transforms: Vec<Transform>,
    /// The outline the next fill or clip takes: version 0 fills an outline
    /// it just read, version 1 clips to it first.
    pending: Option<Shape>,
}

impl Recorder<'_, '_> {
    fn add(&mut self, outline: Outline) -> Shape {
        if let Some([x_min, y_min, x_max, y_max]) = outline.bounds {
            for corner in [
                [x_min, y_min],
                [x_max, y_min],
                [x_min, y_max],
                [x_max, y_max],
            ] {
                let [x, y] = apply(self.transform, corner);
                let bounds = self.painting.bounds.get_or_insert([x, y, x, y]);
                *bounds = [
                    bounds[0].min(x),
                    bounds[1].min(y),
                    bounds[2].max(x),
                    bounds[3].max(y),
                ];
            }
        }
        self.painting.outlines.push(outline);
        Shape {
            outline: self.painting.outlines.len() - 1,
            transform: self.transform,
        }
    }
}

impl<'a> Painter<'a> for Recorder<'_, 'a> {
    fn outline_glyph(&mut self, glyph_id: GlyphId) {
        let outline = Outline::of(self.face, glyph_id.0);
        self.pending = Some(self.add(outline));
    }

    fn paint(&mut self, paint: Paint<'a>) {
        let fill = Fill::of(&paint);
        self.painting.commands.push(Command::Fill {
            shape: self.pending.take(),
            transform: self.transform,
            fill,
        });
    }

    fn push_clip(&mut self) {
        let shape = match self.pending.take() {
            Some(shape) => shape,
            None => self.add(Outline::default()),
        };
        self.painting.commands.push(Command::PushClip(shape));
    }

    fn push_clip_box(&mut self, clip_box: ClipBox) {
        let ClipBox {
            x_min,
            y_min,
            x_max,
            y_max,
        } = clip_box;
        let corners = [
            [x_min, y_min],
            [x_max, y_min],
            [x_max, y_max],
            [x_min, y_max],
        ];
        let outline = Outline {
            segments: (0..4)
                .map(|index| Segment::Line(corners[index], corners[(index + 1) % 4]))
                .collect(),
            bounds: Some([x_min, y_min, x_max, y_max]),
        };
        let shape = self.add(outline);
        self.painting.commands.push(Command::PushClip(shape));
    }

    fn pop_clip(&mut self) {
        self.painting.commands.push(Command::PopClip);
    }

    fn push_layer(&mut self, mode: CompositeMode) {
        self.painting.commands.push(Command::PushLayer(mode));
    }

    fn pop_layer(&mut self) {
        self.painting.commands.push(Command::PopLayer);
    }

    fn push_transform(&mut self, transform: Transform) {
        self.transforms.push(self.transform);
        self.transform = Transform::combine(self.transform, transform);
    }

    fn pop_transform(&mut self) {
        if let Some(transform) = self.transforms.pop() {
            self.transform = transform;
        }
    }
}

impl Fill {
    fn of(paint: &Paint<'_>) -> Self {
        match paint {
            Paint::Solid(color) => Self::Solid(premultiply(*color)),
            Paint::LinearGradient(gradient) => {
                let start = [gradient.x0, gradient.y0];
                let along = [gradient.x1 - gradient.x0, gradient.y1 - gradient.y0];
                // The line through `p0` square to `p0 p2` holds the color line: `p1` projected
                // onto it ends it.
                let normal = [gradient.y2 - gradient.y0, gradient.x0 - gradient.x2];
                let length = dot(normal, normal);
                let end = if length == 0.0 {
                    [gradient.x1, gradient.y1]
                } else {
                    let reach = dot(along, normal) / length;
                    [start[0] + normal[0] * reach, start[1] + normal[1] * reach]
                };
                Self::Linear {
                    start,
                    end,
                    line: ColorLine::of(gradient.stops(0, &[]), gradient.extend),
                }
            }
            Paint::RadialGradient(gradient) => Self::Radial {
                start: [gradient.x0, gradient.y0],
                start_radius: gradient.r0,
                end: [gradient.x1, gradient.y1],
                end_radius: gradient.r1,
                line: ColorLine::of(gradient.stops(0, &[]), gradient.extend),
            },
            // The table gives angles in half turns.
            Paint::SweepGradient(gradient) => Self::Sweep {
                center: [gradient.center_x, gradient.center_y],
                start_angle: gradient.start_angle * 180.0,
                end_angle: gradient.end_angle * 180.0,
                line: ColorLine::of(gradient.stops(0, &[]), gradient.extend),
            },
        }
    }

    /// The color at `at`, in the fill's own space.
    fn at(&self, at: [f32; 2]) -> Color {
        match self {
            Self::Solid(color) => *color,
            Self::Linear { start, end, line } => {
                let along = [end[0] - start[0], end[1] - start[1]];
                let length = dot(along, along);
                if length == 0.0 {
                    return [0.0; 4];
                }
                line.at(dot([at[0] - start[0], at[1] - start[1]], along) / length)
            }
            Self::Radial {
                start,
                start_radius,
                end,
                end_radius,
                line,
            } => radial(*start, *start_radius, *end, *end_radius, at)
                .map_or([0.0; 4], |offset| line.at(offset)),
            Self::Sweep {
                center,
                start_angle,
                end_angle,
                line,
            } => {
                let sweep = end_angle - start_angle;
                if sweep == 0.0 {
                    return [0.0; 4];
                }
                let mut angle = libm::atan2f(at[1] - center[1], at[0] - center[0])
                    * (180.0 / std::f32::consts::PI);
                if angle < 0.0 {
                    angle += 360.0;
                }
                line.at((angle - start_angle) / sweep)
            }
        }
    }
}

/// The largest offset `t` whose circle, centre `start + t (end - start)`
/// and radius `start_radius + t (end_radius - start_radius)` at least zero,
/// passes through `at`: a two-point conical gradient's offset there, if
/// any circle does.
fn radial(
    start: [f32; 2],
    start_radius: f32,
    end: [f32; 2],
    end_radius: f32,
    at: [f32; 2],
) -> Option<f32> {
    let centres = [end[0] - start[0], end[1] - start[1]];
    let from = [at[0] - start[0], at[1] - start[1]];
    let grows = end_radius - start_radius;
    let a = dot(centres, centres) - grows * grows;
    let b = dot(from, centres) + start_radius * grows;
    let c = dot(from, from) - start_radius * start_radius;
    let radius = |offset: f32| start_radius + offset * grows;
    if a == 0.0 {
        if b == 0.0 {
            return None;
        }
        let offset = c / (2.0 * b);
        return (radius(offset) >= 0.0).then_some(offset);
    }
    let discriminant = b * b - a * c;
    if discriminant < 0.0 {
        return None;
    }
    let root = discriminant.sqrt();
    let (first, second) = ((b + root) / a, (b - root) / a);
    let (larger, smaller) = if first >= second {
        (first, second)
    } else {
        (second, first)
    };
    [larger, smaller]
        .into_iter()
        .find(|&offset| radius(offset) >= 0.0)
}

impl ColorLine {
    fn of(
        stops: impl Iterator<Item = ttf_parser::colr::ColorStop>,
        extend: GradientExtend,
    ) -> Self {
        let mut stops = stops
            .map(|stop| (stop.stop_offset, premultiply(stop.color)))
            .collect::<Vec<_>>();
        // Stable, so stops at one offset keep the table's order: a hard edge between them.
        stops.sort_by(|left, right| left.0.total_cmp(&right.0));
        Self { stops, extend }
    }

    /// The color at `offset`, past the stops as `extend` says, interpolated
    /// premultiplied.
    fn at(&self, offset: f32) -> Color {
        let (Some(first), Some(last)) = (self.stops.first(), self.stops.last()) else {
            return [0.0; 4];
        };
        let span = last.0 - first.0;
        let offset = if span > 0.0 {
            let along = (offset - first.0) / span;
            let along = match self.extend {
                GradientExtend::Pad => along.clamp(0.0, 1.0),
                GradientExtend::Repeat => along - along.floor(),
                GradientExtend::Reflect => {
                    let turn = along - 2.0 * (along * 0.5).floor();
                    if turn > 1.0 { 2.0 - turn } else { turn }
                }
            };
            first.0 + along * span
        } else {
            offset
        };
        if offset <= first.0 {
            return first.1;
        }
        // The last stop at or before `offset`, and the one after it.
        let after = self.stops.partition_point(|stop| stop.0 <= offset);
        let Some(&(to, to_color)) = self.stops.get(after) else {
            return last.1;
        };
        let (from, from_color) = self.stops[after - 1];
        let weight = (offset - from) / (to - from);
        std::array::from_fn(|channel| {
            from_color[channel] + (to_color[channel] - from_color[channel]) * weight
        })
    }
}

/// A CBDT or sbix glyph image, premultiplied, placed in font units.
pub(super) struct Bitmap {
    width: usize,
    height: usize,
    pixels: Vec<Color>,
    /// `[x_min, y_min, x_max, y_max]` of the image in font units.
    bounds: [f32; 4],
}

impl Bitmap {
    /// The color image the font has for `glyph` from the strike nearest
    /// above `font_size` pixels an em (else the largest), PNG or BGRA;
    /// `None` when the font has no color bitmaps or none for it.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(super) fn of(face: &ttf_parser::Face<'_>, glyph: u16, font_size: f32) -> Option<Self> {
        let tables = face.tables();
        if tables.cbdt.is_none() && tables.sbix.is_none() {
            return None;
        }
        let pixels_per_em = font_size.ceil().clamp(1.0, f32::from(u16::MAX)) as u16;
        let image = face.glyph_raster_image(GlyphId(glyph), pixels_per_em)?;
        if image.pixels_per_em == 0 {
            return None;
        }
        let (width, height, pixels) = match image.format {
            RasterImageFormat::PNG => decode_png(image.data)?,
            RasterImageFormat::BitmapPremulBgra32 => {
                let (width, height) = (usize::from(image.width), usize::from(image.height));
                let pixels = image
                    .data
                    .get(..width * height * 4)?
                    .as_chunks::<4>()
                    .0
                    .iter()
                    // Premultiplied already.
                    .map(|&[blue, green, red, alpha]| {
                        [red, green, blue, alpha].map(|value| f32::from(value) / 255.0)
                    })
                    .collect();
                (width, height, pixels)
            }
            _ => return None,
        };
        if width == 0 || height == 0 {
            return None;
        }
        // The image's own size, which the table's metrics need not match.
        let units = f32::from(face.units_per_em()) / f32::from(image.pixels_per_em);
        let (x, y) = (f32::from(image.x), f32::from(image.y));
        Some(Self {
            width,
            height,
            pixels,
            bounds: [
                x * units,
                y * units,
                (x + width as f32) * units,
                (y + height as f32) * units,
            ],
        })
    }

    /// `[x_min, y_min, x_max, y_max]` in font units.
    pub(super) fn bounds(&self) -> [f32; 4] {
        self.bounds
    }

    /// `[x_min, y_min, x_max, y_max]` in font units of the image's pixels
    /// with any alpha, which a strike often pads with transparent ones;
    /// `None` when it has none.
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn ink_bounds(&self) -> Option<[f32; 4]> {
        let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
        for (index, pixel) in self.pixels.iter().enumerate() {
            if pixel[3] > 0.0 {
                let (x, y) = (index % self.width, index / self.width);
                (left, top) = (left.min(x), top.min(y));
                (right, bottom) = (right.max(x + 1), bottom.max(y + 1));
            }
        }
        if left > right {
            return None;
        }
        // Image rows run top down; font units up.
        let [x_min, y_min, x_max, y_max] = self.bounds;
        let across = (x_max - x_min) / self.width as f32;
        let down = (y_max - y_min) / self.height as f32;
        Some([
            x_min + left as f32 * across,
            y_max - bottom as f32 * down,
            x_min + right as f32 * across,
            y_max - top as f32 * down,
        ])
    }

    /// The image over `pixels`, premultiplied RGBA8 row by row, with its
    /// origin at `origin` at `scale`: each pixel the mean of the image over
    /// its footprint, at least one image pixel wide, so a strike drawn
    /// smaller is averaged and one drawn larger is interpolated.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    pub(super) fn draw(&self, origin: [f32; 2], scale: f32, pixels: PixelBox) -> Vec<u8> {
        let place = Place::of(origin, scale, pixels);
        let [x_min, y_min, x_max, y_max] = self.bounds;
        // Layout pixels an image pixel, across and down.
        let step = [
            (x_max - x_min) * scale / self.width as f32,
            (y_max - y_min) * scale / self.height as f32,
        ];
        let image_left = origin[0] + x_min * scale;
        let image_top = origin[1] - y_max * scale;
        let columns = taps(place.width, place.left - image_left, step[0], self.width);
        let rows = taps(place.height, place.top - image_top, step[1], self.height);
        let mut rgba8 = vec![0_u8; place.width * place.height * 4];
        for (y, (first_row, row_weights)) in rows.iter().enumerate() {
            for (x, (first_column, column_weights)) in columns.iter().enumerate() {
                let mut sum = [0.0_f32; 4];
                for (row, row_weight) in row_weights.iter().enumerate() {
                    let line = (first_row + row) * self.width;
                    for (column, column_weight) in column_weights.iter().enumerate() {
                        let weight = row_weight * column_weight;
                        let pixel = self.pixels[line + first_column + column];
                        for (total, channel) in sum.iter_mut().zip(pixel) {
                            *total += channel * weight;
                        }
                    }
                }
                let out = &mut rgba8[(y * place.width + x) * 4..][..4];
                for (byte, channel) in out.iter_mut().zip(sum) {
                    *byte = (channel.clamp(0.0, 1.0) * 255.0).round() as u8;
                }
            }
        }
        rgba8
    }
}

/// For each of `count` pixels along an axis, the first image pixel its
/// footprint reaches and the share of the footprint each takes from there.
/// Pixel `i` is centred `offset + i + 0.5` layout pixels past the image's
/// edge, `step` layout pixels an image pixel; image pixels past `length`
/// are transparent, so their share is left out.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn taps(count: usize, offset: f32, step: f32, length: usize) -> Vec<(usize, Vec<f32>)> {
    let half = (0.5 / step).max(0.5);
    (0..count)
        .map(|index| {
            let centre = (offset + index as f32 + 0.5) / step;
            let (low, high) = (centre - half, centre + half);
            let first = low.floor().max(0.0) as usize;
            let last = (high.ceil().max(0.0) as usize).min(length);
            let weights = (first..last)
                .map(|pixel| {
                    let pixel = pixel as f32;
                    ((high.min(pixel + 1.0) - low.max(pixel)).max(0.0)) / (2.0 * half)
                })
                .collect::<Vec<_>>();
            (first.min(last), weights)
        })
        .collect()
}

/// A PNG's pixels, premultiplied, with its width and height.
fn decode_png(data: &[u8]) -> Option<(usize, usize, Vec<Color>)> {
    let mut decoder = png::Decoder::new(Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0_u8; reader.output_buffer_size()?];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let (width, height) = (
        usize::try_from(frame.width).ok()?,
        usize::try_from(frame.height).ok()?,
    );
    let channels = match frame.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return None,
    };
    let pixels = buffer
        .get(..width * height * channels)?
        .chunks_exact(channels)
        .map(|pixel| {
            let [red, green, blue, alpha] = match *pixel {
                [gray] => [gray, gray, gray, 255],
                [gray, alpha] => [gray, gray, gray, alpha],
                [red, green, blue] => [red, green, blue, 255],
                [red, green, blue, alpha] => [red, green, blue, alpha],
                _ => [0; 4],
            };
            premultiply(RgbaColor::new(red, green, blue, alpha))
        })
        .collect();
    Some((width, height, pixels))
}

fn premultiply(color: RgbaColor) -> Color {
    let alpha = f32::from(color.alpha) / 255.0;
    [
        f32::from(color.red) / 255.0 * alpha,
        f32::from(color.green) / 255.0 * alpha,
        f32::from(color.blue) / 255.0 * alpha,
        alpha,
    ]
}

fn dot(left: [f32; 2], right: [f32; 2]) -> f32 {
    left[0] * right[0] + left[1] * right[1]
}

fn apply(transform: Transform, [x, y]: [f32; 2]) -> [f32; 2] {
    [
        transform.a * x + transform.c * y + transform.e,
        transform.b * x + transform.d * y + transform.f,
    ]
}

/// The transform undoing `transform`, if it has one.
fn invert(transform: Transform) -> Option<Transform> {
    let Transform { a, b, c, d, e, f } = transform;
    let determinant = a * d - b * c;
    if determinant == 0.0 || !determinant.is_finite() {
        return None;
    }
    Some(Transform {
        a: d / determinant,
        b: -b / determinant,
        c: -c / determinant,
        d: a / determinant,
        e: (c * f - d * e) / determinant,
        f: (b * e - a * f) / determinant,
    })
}

/// `source` composited onto `backdrop` in `mode`, premultiplied: the
/// Porter-Duff operators and the W3C Compositing and Blending modes the
/// COLR table names.
fn composite(mode: CompositeMode, source: Color, backdrop: Color) -> Color {
    let (source_alpha, backdrop_alpha) = (source[3], backdrop[3]);
    let porter_duff = |from_source: f32, from_backdrop: f32| -> Color {
        std::array::from_fn(|channel| {
            source[channel] * from_source + backdrop[channel] * from_backdrop
        })
    };
    match mode {
        CompositeMode::Clear => [0.0; 4],
        CompositeMode::Source => source,
        CompositeMode::Destination => backdrop,
        CompositeMode::SourceOver => porter_duff(1.0, 1.0 - source_alpha),
        CompositeMode::DestinationOver => porter_duff(1.0 - backdrop_alpha, 1.0),
        CompositeMode::SourceIn => porter_duff(backdrop_alpha, 0.0),
        CompositeMode::DestinationIn => porter_duff(0.0, source_alpha),
        CompositeMode::SourceOut => porter_duff(1.0 - backdrop_alpha, 0.0),
        CompositeMode::DestinationOut => porter_duff(0.0, 1.0 - source_alpha),
        CompositeMode::SourceAtop => porter_duff(backdrop_alpha, 1.0 - source_alpha),
        CompositeMode::DestinationAtop => porter_duff(1.0 - backdrop_alpha, source_alpha),
        CompositeMode::Xor => porter_duff(1.0 - backdrop_alpha, 1.0 - source_alpha),
        CompositeMode::Plus => {
            std::array::from_fn(|channel| (source[channel] + backdrop[channel]).min(1.0))
        }
        _ => blend(mode, source, backdrop),
    }
}

/// `source` over `backdrop` with their colors mixed by a blend mode where
/// both have ink.
fn blend(mode: CompositeMode, source: Color, backdrop: Color) -> Color {
    let (source_alpha, backdrop_alpha) = (source[3], backdrop[3]);
    let straight = |color: Color| -> [f32; 3] {
        if color[3] > 0.0 {
            std::array::from_fn(|channel| color[channel] / color[3])
        } else {
            [0.0; 3]
        }
    };
    let (top, bottom) = (straight(source), straight(backdrop));
    let mixed = match mode {
        CompositeMode::Hue => {
            set_luminosity(set_saturation(top, saturation(bottom)), luminosity(bottom))
        }
        CompositeMode::Saturation => {
            set_luminosity(set_saturation(bottom, saturation(top)), luminosity(bottom))
        }
        CompositeMode::Color => set_luminosity(top, luminosity(bottom)),
        CompositeMode::Luminosity => set_luminosity(bottom, luminosity(top)),
        _ => std::array::from_fn(|channel| separable(mode, bottom[channel], top[channel])),
    };
    let mut out = [0.0; 4];
    for channel in 0..3 {
        out[channel] = source[channel] * (1.0 - backdrop_alpha)
            + backdrop[channel] * (1.0 - source_alpha)
            + source_alpha * backdrop_alpha * mixed[channel];
    }
    out[3] = source_alpha + backdrop_alpha - source_alpha * backdrop_alpha;
    out
}

/// A separable blend mode's color for `backdrop` under `source`, straight.
fn separable(mode: CompositeMode, backdrop: f32, source: f32) -> f32 {
    match mode {
        CompositeMode::Multiply => backdrop * source,
        CompositeMode::Screen => screen(backdrop, source),
        CompositeMode::Overlay => hard_light(source, backdrop),
        CompositeMode::Darken => backdrop.min(source),
        CompositeMode::Lighten => backdrop.max(source),
        CompositeMode::ColorDodge => {
            if backdrop <= 0.0 {
                0.0
            } else if source >= 1.0 {
                1.0
            } else {
                (backdrop / (1.0 - source)).min(1.0)
            }
        }
        CompositeMode::ColorBurn => {
            if backdrop >= 1.0 {
                1.0
            } else if source <= 0.0 {
                0.0
            } else {
                1.0 - ((1.0 - backdrop) / source).min(1.0)
            }
        }
        CompositeMode::HardLight => hard_light(backdrop, source),
        CompositeMode::SoftLight => {
            if source <= 0.5 {
                backdrop - (1.0 - 2.0 * source) * backdrop * (1.0 - backdrop)
            } else {
                let lifted = if backdrop <= 0.25 {
                    ((16.0 * backdrop - 12.0) * backdrop + 4.0) * backdrop
                } else {
                    backdrop.sqrt()
                };
                backdrop + (2.0 * source - 1.0) * (lifted - backdrop)
            }
        }
        CompositeMode::Difference => (backdrop - source).abs(),
        CompositeMode::Exclusion => backdrop + source - 2.0 * backdrop * source,
        _ => source,
    }
}

fn screen(backdrop: f32, source: f32) -> f32 {
    backdrop + source - backdrop * source
}

fn hard_light(backdrop: f32, source: f32) -> f32 {
    if source <= 0.5 {
        backdrop * 2.0 * source
    } else {
        screen(backdrop, 2.0 * source - 1.0)
    }
}

fn luminosity(color: [f32; 3]) -> f32 {
    0.3 * color[0] + 0.59 * color[1] + 0.11 * color[2]
}

/// `color` moved to `luminosity`, brought back into gamut by its hue.
fn set_luminosity(color: [f32; 3], luminosity_wanted: f32) -> [f32; 3] {
    let shift = luminosity_wanted - luminosity(color);
    let color = color.map(|channel| channel + shift);
    let level = luminosity(color);
    let lowest = color[0].min(color[1]).min(color[2]);
    let highest = color[0].max(color[1]).max(color[2]);
    let mut color = color;
    if lowest < 0.0 {
        color = color.map(|channel| level + (channel - level) * level / (level - lowest));
    }
    if highest > 1.0 {
        color = color.map(|channel| level + (channel - level) * (1.0 - level) / (highest - level));
    }
    color
}

fn saturation(color: [f32; 3]) -> f32 {
    color[0].max(color[1]).max(color[2]) - color[0].min(color[1]).min(color[2])
}

/// `color` with its channels spread to `saturation_wanted`, in their order.
fn set_saturation(color: [f32; 3], saturation_wanted: f32) -> [f32; 3] {
    let mut order = [0, 1, 2];
    order.sort_by(|&left, &right| color[left].total_cmp(&color[right]));
    let [low, middle, high] = order;
    let mut out = [0.0; 3];
    let range = color[high] - color[low];
    if range > 0.0 {
        out[middle] = (color[middle] - color[low]) * saturation_wanted / range;
        out[high] = saturation_wanted;
    }
    out
}

#[cfg(test)]
#[path = "color_tests.rs"]
mod tests;
