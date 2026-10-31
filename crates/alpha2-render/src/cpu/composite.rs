//! The composite node: layers over a background, in order, each pixel's
//! result stored as the intermediate holds it before the next layer reads
//! it (composite.wesl).
//!
//! A layer that is not rotated samples through tap tables: its transform's
//! derivatives are constants, its edge coverage is a product of an x and a y
//! term, and a bilinear sample is separable, so the shader's filter loop
//! reduces to weights per output column and per output row, built once per
//! layer. A rotated layer runs the shader's loop literally.

use super::blend::composite as blend;
use super::half;
use super::image::{CpuImage, PixelBox, Pixels};
use super::workers::Workers;
use crate::{BlendMode, BlendSpace, Color, LayerTransform, RenderError, RenderSize};

/// The alpha from which a source pixel under the normal mode counts as
/// nearly opaque. Each filter sample's edge coverage subtracts two nearly
/// equal positions in `f32`, as the shader does, so a minified opaque layer
/// reaches the composite up to a few 1e-4 short of opaque (on the GPU too).
/// From 2^-8 short, the backdrop that shows through is mixed in the encoded
/// space instead of in linear light. Over a contribution that small the
/// curve's bend changes the result by less than 1e-3, under a tenth of an
/// 8-bit code.
const NEARLY_OPAQUE: f32 = 1.0 - 1.0 / 256.0;

/// One layer as the composite node draws it.
pub(crate) struct LayerInput<'a> {
    pub(crate) image: &'a CpuImage,
    pub(crate) visual: LayerTransform,
    pub(crate) blend_mode: BlendMode,
}

/// The composite node's settings.
#[derive(Copy, Clone)]
pub(crate) struct CompositeSettings {
    pub(crate) size: RenderSize,
    pub(crate) background: Option<Color>,
    pub(crate) blend_space: BlendSpace,
    pub(crate) max_filter_samples: u32,
}

/// Composites `layers` in order over the background.
///
/// # Errors
/// Returns [`RenderError::InvalidVisual`] for a layer whose transform or
/// opacity is not valid.
pub(crate) fn composite(
    workers: &Workers,
    layers: &[LayerInput<'_>],
    settings: CompositeSettings,
) -> Result<CpuImage, RenderError> {
    let size = settings.size;
    for layer in layers {
        crate::composite::validate_visual(layer.visual)?;
    }
    let prepared = layers
        .iter()
        .filter(|layer| {
            !crate::composite::is_degenerate(layer.visual)
                && !crate::composite::is_invisible(layer.visual)
        })
        .filter_map(|layer| {
            let bounds = crate::composite::layer_bounds(layer.visual, size)?;
            let bounds = PixelBox {
                x: bounds.x,
                y: bounds.y,
                width: bounds.width,
                height: bounds.height,
            };
            let bounds = bounds.intersect(content_bounds(layer, settings.size)?);
            (!bounds.is_empty()).then(|| PreparedLayer::new(layer, bounds, settings))
        })
        .collect::<Vec<_>>();
    let background =
        crate::composite::stored_background(settings.background, wgpu::TextureFormat::Rgba16Float);
    let bounds = if settings.background.is_some() {
        PixelBox::whole(size)
    } else {
        prepared
            .iter()
            .fold(PixelBox::EMPTY, |union, layer| union.union(layer.bounds))
    };
    if bounds.is_empty() {
        return Ok(CpuImage::transparent(size));
    }
    let fill = half::store4(background);
    let mut pixels = vec![fill; bounds.pixels()];
    let width = bounds.width as usize;
    let linear_light = settings.blend_space == BlendSpace::LinearLight;
    workers.bands(&mut pixels, width, |first_row, band| {
        // Each layer's filtered source rows, made once for the band.
        let mut caches = prepared
            .iter()
            .map(|_| RowCache::default())
            .collect::<Vec<_>>();
        for (index, line) in band.chunks_mut(width).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let y = bounds.y + (first_row + index) as u32;
            for (layer, cache) in prepared.iter().zip(&mut caches) {
                layer.draw_row(y, bounds, line, cache, linear_light);
            }
        }
    });
    Ok(CpuImage::new(size, bounds, Pixels::Half(pixels)))
}

/// The output pixels a layer's samples can find anything in: its source's
/// content rectangle mapped as `layer_bounds` maps the whole source, grown by
/// the reach of the filter (two source texels, at the output's scale). Past
/// it every sample reads transparency, which composites to the backdrop.
/// `None` when that misses the target.
fn content_bounds(layer: &LayerInput<'_>, size: RenderSize) -> Option<PixelBox> {
    let image = layer.image;
    let content = image.bounds();
    if content.is_empty() {
        return None;
    }
    let source = image.size();
    if content == PixelBox::whole(source) {
        return Some(PixelBox::whole(size));
    }
    let visual = layer.visual;
    let (width, height) = (f64::from(size.width), f64::from(size.height));
    let texel = (f64::from(visual.scale[0]).abs() * width / f64::from(source.width))
        .max(f64::from(visual.scale[1]).abs() * height / f64::from(source.height));
    let margin = 4.0 + 2.0 * texel.max(1.0);
    let (turn_sin, turn_cos) = libm::sincos(f64::from(visual.rotation_degrees).to_radians());
    let corner = |u: f64, v: f64| {
        let x = (u - f64::from(visual.anchor[0])) * f64::from(visual.scale[0]) * width;
        let y = (v - f64::from(visual.anchor[1])) * f64::from(visual.scale[1]) * height;
        (
            f64::from(visual.position[0]) * width + turn_cos * x - turn_sin * y,
            f64::from(visual.position[1]) * height + turn_sin * x + turn_cos * y,
        )
    };
    let (u0, u1) = (
        f64::from(content.x) / f64::from(source.width),
        f64::from(content.right()) / f64::from(source.width),
    );
    let (v0, v1) = (
        f64::from(content.y) / f64::from(source.height),
        f64::from(content.bottom()) / f64::from(source.height),
    );
    let corners = [
        corner(u0, v0),
        corner(u1, v0),
        corner(u0, v1),
        corner(u1, v1),
    ];
    let fold = |pick: fn(&(f64, f64)) -> f64, extreme: fn(f64, f64) -> f64, start: f64| {
        corners.iter().map(pick).fold(start, extreme)
    };
    let left = (fold(|c| c.0, f64::min, f64::INFINITY) - margin)
        .floor()
        .max(0.0);
    let top = (fold(|c| c.1, f64::min, f64::INFINITY) - margin)
        .floor()
        .max(0.0);
    let right = (fold(|c| c.0, f64::max, f64::NEG_INFINITY) + margin)
        .ceil()
        .min(width);
    let bottom = (fold(|c| c.1, f64::max, f64::NEG_INFINITY) + margin)
        .ceil()
        .min(height);
    if !(right > left && bottom > top) {
        return None;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some(PixelBox {
        x: left as u32,
        y: top as u32,
        width: (right - left) as u32,
        height: (bottom - top) as u32,
    })
}

/// One output column's or row's filter: the source texels it reads and
/// their weights, and whether the layer's coverage reaches it at all.
#[derive(Default)]
struct AxisTaps {
    /// Per output position, the range of `taps` it reads; empty where the
    /// layer's coverage is zero.
    spans: Vec<(u32, u32)>,
    taps: Vec<(u32, f32)>,
}

impl AxisTaps {
    fn taps(&self, position: usize) -> &[(u32, f32)] {
        let (start, end) = self.spans[position];
        &self.taps[start as usize..end as usize]
    }

    /// The first and last source texel any position reads.
    fn reach(&self) -> Option<(u32, u32)> {
        let first = self.taps.iter().map(|(index, _)| *index).min()?;
        let last = self.taps.iter().map(|(index, _)| *index).max()?;
        Some((first, last))
    }
}

/// A layer's source rows filtered horizontally for every output column,
/// made the first time a band's row reads them.
#[derive(Default)]
struct RowCache {
    rows: Vec<(u32, Vec<[f32; 4]>)>,
    decoded: Vec<[f32; 4]>,
}

impl RowCache {
    fn ensure(&mut self, row: u32, image: &CpuImage, columns: &AxisTaps) {
        if self.rows.iter().any(|(held, _)| *held == row) {
            return;
        }
        let Some((first, last)) = columns.reach() else {
            return;
        };
        self.decoded.resize((last - first + 1) as usize, [0.0; 4]);
        image.decode_row(row, first, &mut self.decoded);
        let filtered = (0..columns.spans.len())
            .map(|column| {
                let mut sum = [0.0_f32; 4];
                for &(texel, weight) in columns.taps(column) {
                    let value = self.decoded[(texel - first) as usize];
                    for channel in 0..4 {
                        sum[channel] += value[channel] * weight;
                    }
                }
                sum
            })
            .collect();
        self.rows.push((row, filtered));
    }

    fn row(&self, row: u32) -> &[[f32; 4]] {
        self.rows
            .iter()
            .find(|(held, _)| *held == row)
            .map_or(&[], |(_, filtered)| filtered.as_slice())
    }
}

enum Sampler {
    /// An unrotated layer: tap tables for its columns and rows.
    Separable { columns: AxisTaps, rows: AxisTaps },
    /// A rotated layer, sampled as the shader samples it.
    General(General),
}

/// The shader's per-pixel mapping for a rotated layer.
struct General {
    position: [f32; 2],
    anchor: [f32; 2],
    scale: [f32; 2],
    cosine: f32,
    sine: f32,
    output: [f32; 2],
    dx: [f32; 2],
    dy: [f32; 2],
    grid: [f32; 2],
    footprint: [f32; 2],
}

struct PreparedLayer<'a> {
    image: &'a CpuImage,
    /// Every half float's value, to read the backdrop with.
    halves: &'static [f32; 65_536],
    blend_mode: BlendMode,
    opacity: f32,
    bounds: PixelBox,
    sampler: Sampler,
}

/// composite.wesl's `rectangle_coverage` on one axis.
#[inline]
fn coverage(position: f32, footprint: f32) -> f32 {
    let width = footprint.max(0.000_000_1);
    let lower = (position - width * 0.5).max(0.0);
    let upper = (position + width * 0.5).min(1.0);
    ((upper - lower) / width).clamp(0.0, 1.0)
}

/// The filter grid of `filtered_source`: samples along each axis.
#[allow(clippy::cast_precision_loss)]
fn filter_grid(dx: [f32; 2], dy: [f32; 2], dimensions: [f32; 2], budget: u32) -> [f32; 2] {
    let length = |v: [f32; 2]| (v[0] * v[0] + v[1] * v[1]).sqrt();
    let wanted_x = (length([dx[0] * dimensions[0], dx[1] * dimensions[1]]) - 0.0001)
        .ceil()
        .clamp(1.0, 8.0);
    let wanted_y = (length([dy[0] * dimensions[0], dy[1] * dimensions[1]]) - 0.0001)
        .ceil()
        .clamp(1.0, 8.0);
    let budget = (budget as f32).clamp(1.0, 64.0);
    let nx = wanted_x.min(budget);
    let ny = wanted_y.min((budget / nx).floor().max(1.0));
    [nx, ny]
}

impl<'a> PreparedLayer<'a> {
    #[allow(clippy::cast_precision_loss)]
    fn new(layer: &LayerInput<'a>, bounds: PixelBox, settings: CompositeSettings) -> Self {
        let visual = layer.visual;
        let output = [settings.size.width as f32, settings.size.height as f32];
        let dimensions = [
            layer.image.size().width as f32,
            layer.image.size().height as f32,
        ];
        let angle = visual.rotation_degrees.to_radians();
        let (sine, cosine) = (libm::sinf(angle), libm::cosf(angle));
        let scale = visual.scale;
        // The derivatives of the shader's mapping, which is affine: per output pixel.
        let dx = [cosine / output[0] / scale[0], -sine / output[1] / scale[1]];
        let dy = [sine / output[0] / scale[0], cosine / output[1] / scale[1]];
        let grid = filter_grid(dx, dy, dimensions, settings.max_filter_samples);
        let footprint = [
            dx[0].abs() / grid[0] + dy[0].abs() / grid[1],
            dx[1].abs() / grid[0] + dy[1].abs() / grid[1],
        ];
        let sampler = if sine == 0.0 {
            Sampler::Separable {
                columns: axis_taps(
                    bounds.x..bounds.right(),
                    output[0],
                    visual.position[0],
                    scale[0],
                    visual.anchor[0],
                    dx[0],
                    grid[0],
                    footprint[0],
                    dimensions[0],
                ),
                rows: axis_taps(
                    bounds.y..bounds.bottom(),
                    output[1],
                    visual.position[1],
                    scale[1],
                    visual.anchor[1],
                    dy[1],
                    grid[1],
                    footprint[1],
                    dimensions[1],
                ),
            }
        } else {
            Sampler::General(General {
                position: visual.position,
                anchor: visual.anchor,
                scale,
                cosine,
                sine,
                output,
                dx,
                dy,
                grid,
                footprint,
            })
        };
        Self {
            image: layer.image,
            halves: half::table(),
            blend_mode: layer.blend_mode,
            opacity: visual.opacity,
            bounds,
            sampler,
        }
    }

    /// Composites this layer into row `y` of the output, `line`, which
    /// holds the output's `bounds` columns.
    fn draw_row(
        &self,
        y: u32,
        bounds: PixelBox,
        line: &mut [[u16; 4]],
        cache: &mut RowCache,
        linear_light: bool,
    ) {
        if y < self.bounds.y || y >= self.bounds.bottom() {
            return;
        }
        let first = self.bounds.x;
        match &self.sampler {
            Sampler::Separable { columns, rows } => {
                let row_taps = rows.taps((y - self.bounds.y) as usize);
                if row_taps.is_empty() {
                    return;
                }
                for &(row, _) in row_taps {
                    cache.ensure(row, self.image, columns);
                }
                // At most eight samples of two texels each: the rows this one reads.
                let mut lines: [(&[[f32; 4]], f32); 32] = [(&[], 0.0); 32];
                for (slot, &(row, weight)) in lines.iter_mut().zip(row_taps) {
                    *slot = (cache.row(row), weight);
                }
                let lines = &lines[..row_taps.len().min(32)];
                for x in first..self.bounds.right() {
                    let column = (x - first) as usize;
                    if columns.spans[column].0 == columns.spans[column].1 {
                        continue;
                    }
                    // The rows' horizontal sums, then the vertical one, in the order
                    // a per-pixel filter sums them.
                    let mut color = [0.0_f32; 4];
                    for &(filtered, row_weight) in lines {
                        let value = filtered[column];
                        for channel in 0..4 {
                            color[channel] += value[channel] * row_weight;
                        }
                    }
                    let out = &mut line[(x - bounds.x) as usize];
                    *out = self.composite_pixel(*out, color, linear_light);
                }
            }
            Sampler::General(general) => {
                for x in first..self.bounds.right() {
                    let Some(color) = general.sample(self.image, x, y) else {
                        continue;
                    };
                    let out = &mut line[(x - bounds.x) as usize];
                    *out = self.composite_pixel(*out, color, linear_light);
                }
            }
        }
    }

    #[inline]
    fn composite_pixel(&self, backdrop: [u16; 4], color: [f32; 4], linear_light: bool) -> [u16; 4] {
        let source = color.map(|value| value * self.opacity);
        // A transparent source leaves the backdrop: the blend returns it through the curve
        // round trip, an identity up to `pow`'s rounding.
        if source[3] <= 0.0 {
            return backdrop;
        }
        // A (nearly) opaque source under the normal mode: the blend's curve round trip is an
        // identity up to `pow`'s rounding, and what shows of the backdrop is mixed in directly.
        if self.blend_mode == BlendMode::Normal && source[3] >= NEARLY_OPAQUE {
            let below = backdrop.map(|bits| self.halves[usize::from(bits)]);
            let rest = 1.0 - source[3].min(1.0);
            return [
                half::store((source[0] + below[0] * rest).clamp(0.0, 1.0)),
                half::store((source[1] + below[1] * rest).clamp(0.0, 1.0)),
                half::store((source[2] + below[2] * rest).clamp(0.0, 1.0)),
                half::store((source[3] + below[3] * rest).min(1.0)),
            ];
        }
        self.blend_pixel(backdrop, source, linear_light)
    }

    /// The full blend, kept out of line: most pixels take a shortcut above.
    #[inline(never)]
    fn blend_pixel(&self, backdrop: [u16; 4], source: [f32; 4], linear_light: bool) -> [u16; 4] {
        half::store4(blend(
            backdrop.map(|bits| self.halves[usize::from(bits)]),
            source,
            self.blend_mode,
            linear_light,
        ))
    }
}

/// The taps of every output position in `range` along one axis of an
/// unrotated layer.
#[allow(clippy::too_many_arguments, clippy::cast_precision_loss)]
fn axis_taps(
    range: std::ops::Range<u32>,
    output: f32,
    position: f32,
    scale: f32,
    anchor: f32,
    derivative: f32,
    grid: f32,
    footprint: f32,
    dimension: f32,
) -> AxisTaps {
    let mut taps = AxisTaps::default();
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let (samples, last) = (grid as u32, dimension as i64 - 1);
    for pixel in range {
        #[allow(clippy::cast_possible_truncation)]
        let start = taps.taps.len() as u32;
        let uv = (pixel as f32 + 0.5) / output;
        let source = (uv - position) * output / output / scale + anchor;
        // Whether the layer reaches this column at all: the shader's gate.
        if coverage(source, derivative.abs()) <= 0.0 {
            taps.spans.push((start, start));
            continue;
        }
        for sample in 0..samples {
            let offset = (sample as f32 + 0.5) / grid - 0.5;
            let point = source + derivative * offset;
            let weight = coverage(point, footprint) / grid;
            if weight <= 0.0 {
                continue;
            }
            let texel = point * dimension - 0.5;
            let origin = texel.floor();
            let fraction = texel - origin;
            #[allow(clippy::cast_possible_truncation)]
            let origin = origin as i64;
            for (index, tap_weight) in [(origin, 1.0 - fraction), (origin + 1, fraction)] {
                if tap_weight <= 0.0 {
                    continue;
                }
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let index = index.clamp(0, last) as u32;
                let weight = weight * tap_weight;
                match taps.taps[start as usize..]
                    .iter_mut()
                    .find(|(existing, _)| *existing == index)
                {
                    Some((_, existing)) => *existing += weight,
                    None => taps.taps.push((index, weight)),
                }
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        taps.spans.push((start, taps.taps.len() as u32));
    }
    taps
}

impl General {
    /// The layer's filtered sample at output pixel `(x, y)`, or `None` where
    /// its coverage is zero.
    #[allow(clippy::cast_precision_loss)]
    fn sample(&self, image: &CpuImage, x: u32, y: u32) -> Option<[f32; 4]> {
        let uv = [
            (x as f32 + 0.5) / self.output[0],
            (y as f32 + 0.5) / self.output[1],
        ];
        let pixel_offset = [
            (uv[0] - self.position[0]) * self.output[0],
            (uv[1] - self.position[1]) * self.output[1],
        ];
        let rotated = [
            self.cosine * pixel_offset[0] + self.sine * pixel_offset[1],
            -self.sine * pixel_offset[0] + self.cosine * pixel_offset[1],
        ];
        let source = [
            rotated[0] / self.output[0] / self.scale[0] + self.anchor[0],
            rotated[1] / self.output[1] / self.scale[1] + self.anchor[1],
        ];
        let gate = coverage(source[0], self.dx[0].abs() + self.dy[0].abs())
            * coverage(source[1], self.dx[1].abs() + self.dy[1].abs());
        if gate <= 0.0 {
            return None;
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (nx, ny) = (self.grid[0] as u32, self.grid[1] as u32);
        let mut color = [0.0_f32; 4];
        for row in 0..ny {
            for column in 0..nx {
                let offset = [
                    (column as f32 + 0.5) / self.grid[0] - 0.5,
                    (row as f32 + 0.5) / self.grid[1] - 0.5,
                ];
                let point = [
                    source[0] + self.dx[0] * offset[0] + self.dy[0] * offset[1],
                    source[1] + self.dx[1] * offset[0] + self.dy[1] * offset[1],
                ];
                let weight =
                    coverage(point[0], self.footprint[0]) * coverage(point[1], self.footprint[1]);
                if weight <= 0.0 {
                    continue;
                }
                let sample = image.sample(point[0], point[1]);
                for channel in 0..4 {
                    color[channel] += sample[channel] * weight;
                }
            }
        }
        let samples = self.grid[0] * self.grid[1];
        Some(color.map(|value| value / samples))
    }
}
