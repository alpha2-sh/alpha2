//! Vector scenes on the CPU: the GPU vector pass (vector.rs) stage by
//! stage.
//!
//! Each fill's tile coverage comes from [`alpha2_geometry::raster`], as on
//! the GPU. Its full and partial tiles are painted at each pixel's centre
//! through the paint's inverse transform, scaled by coverage and opacity,
//! and blended source-over (`One, 1 - Sa`), each write stored as the
//! half-float target stores it; empty tiles are skipped. A group layer is
//! drawn into a picture of its own and composited at its opacity.
//!
//! Where the GPU draws in linear light (the graph blends in linear light;
//! half floats always blend here), the scene blends in linear light and is
//! encoded once at the end, as the GPU's encode pass does. Otherwise each
//! fill's color is encoded first and blending happens in that space.
//!
//! The picture keeps only the drawn tiles' rectangle, so a small shape on a
//! large canvas stays cheap.

use std::sync::{Arc, Mutex, PoisonError};

use alpha2_geometry::raster::{
    CoverageCache, RasterScratch, TILE, TileCoverage, TileState, coverage_key, rasterize,
};
use alpha2_geometry::{IRect, PaintKind, ScenePaint, VectorOp, VectorScene};

use super::half;
use super::image::{CpuImage, PixelBox, Pixels};
use super::paint::{self, Paint};
use super::workers::Workers;
use crate::{BlendSpace, MAX_GENERATOR_STOPS, RenderError, RenderSize};

type Rgba = [f32; 4];

/// [`TILE`] as coordinates count.
const TILE_EDGE: i64 = 16;

const _: () = assert!(TILE == 16, "tiles are 16 pixels across");

/// Draws `scene` at `size`.
///
/// # Errors
/// Returns [`RenderError::InvalidGraph`] for a layer ended that was never
/// begun, as the GPU pass does.
pub(crate) fn draw(
    workers: &Workers,
    cache: &Mutex<CoverageCache>,
    scene: &VectorScene,
    size: RenderSize,
    blend_space: BlendSpace,
) -> Result<CpuImage, RenderError> {
    if scene.fill_count() == 0 {
        return Ok(CpuImage::transparent(size));
    }
    let linear = blend_space == BlendSpace::LinearLight;
    let fills = scene
        .ops
        .iter()
        .filter_map(|op| match op {
            VectorOp::Fill {
                path, rule, bounds, ..
            } => Some((path, *rule, *bounds)),
            _ => None,
        })
        .collect::<Vec<_>>();
    // Coverage kept between frames by its path's key, as the GPU pass keeps it; what is not
    // held is rasterized in parallel.
    let keys = fills
        .iter()
        .map(|(path, rule, bounds)| coverage_key(path, *rule, *bounds))
        .collect::<Vec<_>>();
    let held = {
        let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
        keys.iter().map(|key| cache.get(*key)).collect::<Vec<_>>()
    };
    let drawn = workers.map(fills.len(), |index| {
        held[index].clone().unwrap_or_else(|| {
            let (path, rule, bounds) = fills[index];
            Arc::new(rasterize(path, rule, bounds, &mut RasterScratch::default()))
        })
    });
    {
        let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
        for ((key, coverage), held) in keys.iter().zip(&drawn).zip(&held) {
            if held.is_none() {
                cache.insert(*key, Arc::clone(coverage));
            }
        }
    }
    let mut coverage = drawn.into_iter();
    let mut steps = Vec::with_capacity(scene.ops.len());
    let mut open = 0_usize;
    let mut area = PixelBox::EMPTY;
    for op in &scene.ops {
        match op {
            VectorOp::PushLayer { bounds, opacity } => {
                open += 1;
                steps.push(Step::Push {
                    bounds: scissor(*bounds, size),
                    opacity: *opacity,
                });
            }
            VectorOp::PopLayer => {
                open = open.checked_sub(1).ok_or(RenderError::InvalidGraph)?;
                steps.push(Step::Pop);
            }
            VectorOp::Fill { paint, opacity, .. } => {
                let coverage = coverage.next().ok_or(RenderError::InvalidGraph)?;
                let fill = FillPlan::new(coverage, paint, *opacity, linear, size);
                area = area.union(fill.drawn);
                steps.push(Step::Fill(Box::new(fill)));
            }
        }
    }
    // The encode pass writes only within the scene's bounds.
    let encoded_area = scissor(scene.bounds, size);
    if linear {
        area = area.intersect(encoded_area);
    }
    if area.is_empty() {
        return Ok(CpuImage::transparent(size));
    }
    let width = area.width as usize;
    let mut pixels = vec![[0_u16; 4]; area.pixels()];
    workers.bands(&mut pixels, width, |first_row, band| {
        #[allow(clippy::cast_possible_truncation)]
        let rows = PixelBox {
            x: area.x,
            y: area.y + first_row as u32,
            width: area.width,
            height: (band.len() / width) as u32,
        };
        let base = draw_band(&steps, rows);
        for (row, (out, line)) in band.chunks_mut(width).zip(base.chunks(width)).enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let y = rows.y + row as u32;
            for (column, (out, value)) in out.iter_mut().zip(line).enumerate() {
                #[allow(clippy::cast_possible_truncation)]
                let x = rows.x + column as u32;
                *out = if linear {
                    if encoded_area.contains(x, y) {
                        half::store4(paint::encode(*value))
                    } else {
                        [0; 4]
                    }
                } else {
                    half::store4(*value)
                };
            }
        }
    });
    Ok(CpuImage::new(size, area, Pixels::Half(pixels)))
}

/// What the pass draws, op by op.
enum Step {
    Fill(Box<FillPlan>),
    /// A layer begins: its contents are composited within `bounds` at
    /// `opacity` when it ends.
    Push {
        bounds: PixelBox,
        opacity: f32,
    },
    Pop,
}

/// Draws `steps` over the pixels `rows` (whole rows of the picture's
/// area), returning the base picture's values there: half floats, as the
/// target holds them.
fn draw_band(steps: &[Step], rows: PixelBox) -> Vec<Rgba> {
    let pixels = rows.pixels();
    // Each open target: its pixels, whether anything is drawn into it yet,
    // and how it ends.
    let mut stack: Vec<(Vec<Rgba>, bool, PixelBox, f32)> =
        vec![(vec![[0.0; 4]; pixels], false, PixelBox::EMPTY, 1.0)];
    let mut spare: Vec<Vec<Rgba>> = Vec::new();
    for step in steps {
        match step {
            Step::Fill(fill) => {
                let (target, drawn, _, _) = stack.last_mut().expect("the base is on the stack");
                *drawn |= fill.draw(target, rows);
            }
            Step::Push { bounds, opacity } => {
                let layer = spare.pop().unwrap_or_else(|| vec![[0.0; 4]; pixels]);
                stack.push((layer, false, *bounds, *opacity));
            }
            Step::Pop => {
                let (mut layer, drawn, bounds, opacity) =
                    stack.pop().expect("layers are balanced before drawing");
                let (parent, parent_drawn, _, _) =
                    stack.last_mut().expect("the base is on the stack");
                // An empty layer composites to what is there: skipped.
                if drawn {
                    *parent_drawn |= composite(parent, &layer, rows, bounds, opacity);
                    layer.fill([0.0; 4]);
                }
                spare.push(layer);
            }
        }
    }
    // Layers still open at the end are never composited, as on the GPU.
    stack.truncate(1);
    stack.pop().map(|(base, ..)| base).unwrap_or_default()
}

/// `layer` at `opacity` over `target` within `bounds` (`fs_layer`).
/// Returns whether it wrote anything.
fn composite(
    target: &mut [Rgba],
    layer: &[Rgba],
    rows: PixelBox,
    bounds: PixelBox,
    opacity: f32,
) -> bool {
    let inside = bounds.intersect(rows);
    if inside.is_empty() {
        return false;
    }
    let width = rows.width as usize;
    for y in inside.y..inside.bottom() {
        let start = (y - rows.y) as usize * width + (inside.x - rows.x) as usize;
        let range = start..start + inside.width as usize;
        for (dst, src) in target[range.clone()].iter_mut().zip(&layer[range]) {
            if src[3] == 0.0 && src[0] == 0.0 && src[1] == 0.0 && src[2] == 0.0 {
                continue;
            }
            over(
                dst,
                [
                    src[0] * opacity,
                    src[1] * opacity,
                    src[2] * opacity,
                    src[3] * opacity,
                ],
            );
        }
    }
    true
}

/// `src` over `dst` premultiplied (`One, 1 - Sa`), computed in `f32` and
/// stored as a half-float target holds it.
///
/// A GPU blends in fixed-function hardware whose internal precision varies:
/// this model is exact on lavapipe, llvmpipe, WARP and Metal and within 16
/// half-float steps (under a quarter of a code) on NVIDIA, RADV and an Intel
/// integrated GPU, which no single blend model fits better. The CPU keeps one model so its
/// output is the same everywhere.
#[inline]
pub(crate) fn over(dst: &mut Rgba, src: Rgba) {
    let keep = 1.0 - src[3];
    for channel in 0..4 {
        dst[channel] = half::load(half::store(src[channel] + dst[channel] * keep));
    }
}

/// One fill, ready to draw: its coverage and paint as the tile shader reads
/// them.
struct FillPlan {
    coverage: Arc<TileCoverage>,
    paint: Paint,
    /// Device pixels to paint-space pixels: `x = m[0] x + m[1] y + m[2]`,
    /// `y = m[3] x + m[4] y + m[5]`, narrowed to `f32` as the uniform holds it.
    inverse: [f32; 6],
    space: [f32; 2],
    opacity: f32,
    /// Encode each color for storage before blending.
    encoded: bool,
    /// The color drawn when it is the same at every pixel.
    constant: Option<Rgba>,
    /// The canvas pixels its non-empty tiles cover.
    drawn: PixelBox,
}

impl FillPlan {
    fn new(
        coverage: Arc<TileCoverage>,
        scene_paint: &ScenePaint,
        opacity: f32,
        linear: bool,
        size: RenderSize,
    ) -> Self {
        let kind = match scene_paint.kind {
            PaintKind::Solid => paint::KIND_SOLID,
            PaintKind::Linear => paint::KIND_LINEAR,
            PaintKind::Radial => paint::KIND_RADIAL,
            PaintKind::Conic => paint::KIND_CONIC,
        };
        let mut offsets = [0.0; MAX_GENERATOR_STOPS];
        let mut colors = [[0.0; 4]; MAX_GENERATOR_STOPS];
        for (index, stop) in scene_paint
            .stops
            .iter()
            .take(MAX_GENERATOR_STOPS)
            .enumerate()
        {
            offsets[index] = stop.offset;
            colors[index] = stop.color;
        }
        let paint = Paint::new(
            kind,
            scene_paint.a,
            scene_paint.b,
            scene_paint.spread,
            scene_paint.stops.len(),
            offsets,
            colors,
        );
        let [xx, yx, xy, yy, x0, y0] = scene_paint.inverse.as_coeffs();
        #[allow(clippy::cast_possible_truncation)]
        let inverse = [xx, xy, x0, yx, yy, y0].map(|value| value as f32);
        let encoded = !linear;
        let constant = paint
            .constant()
            .map(|color| if encoded { paint::encode(color) } else { color });
        let drawn = drawn_area(&coverage, size);
        Self {
            coverage,
            paint,
            inverse,
            space: scene_paint.space,
            opacity,
            encoded,
            constant,
            drawn,
        }
    }

    /// The color `fs_tile` paints at pixel `(x, y)`, before coverage.
    #[inline]
    #[allow(clippy::cast_precision_loss)]
    fn color(&self, x: u32, y: u32) -> Rgba {
        if let Some(color) = self.constant {
            return color;
        }
        let device = [x as f32 + 0.5, y as f32 + 0.5];
        let m = &self.inverse;
        let painted = [
            m[0] * device[0] + m[1] * device[1] + m[2],
            m[3] * device[0] + m[4] * device[1] + m[5],
        ];
        let res = self.space;
        let color = self
            .paint
            .at([painted[0] / res[0], painted[1] / res[1]], res);
        if self.encoded {
            paint::encode(color)
        } else {
            color
        }
    }

    /// Draws its tiles within `rows` into `target`, which holds `rows`.
    /// Returns whether it drew anything there.
    fn draw(&self, target: &mut [Rgba], rows: PixelBox) -> bool {
        let inside = self.drawn.intersect(rows);
        if inside.is_empty() {
            return false;
        }
        let coverage = &self.coverage;
        let (tiles_x, tiles_y) = coverage.size;
        let tile = TILE_EDGE;
        let width = rows.width as usize;
        for ty in 0..tiles_y {
            let top = (i64::from(coverage.origin.1) + i64::from(ty)) * tile;
            let (y0, y1) = (
                top.max(i64::from(inside.y)),
                (top + tile).min(i64::from(inside.bottom())),
            );
            if y0 >= y1 {
                continue;
            }
            for tx in 0..tiles_x {
                let state = coverage.tiles[(ty * tiles_x + tx) as usize];
                let bytes = match state {
                    TileState::Empty => continue,
                    TileState::Full => None,
                    TileState::Partial(index) => Some(&coverage.partial[index as usize]),
                };
                let left = (i64::from(coverage.origin.0) + i64::from(tx)) * tile;
                let (x0, x1) = (
                    left.max(i64::from(inside.x)),
                    (left + tile).min(i64::from(inside.right())),
                );
                if x0 >= x1 {
                    continue;
                }
                // Within the canvas, so non-negative and in range.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                for y in y0 as u32..y1 as u32 {
                    let line = (y - rows.y) as usize * width;
                    let local_y = (i64::from(y) - top) as usize;
                    for x in x0 as u32..x1 as u32 {
                        let coverage = match bytes {
                            None => 1.0,
                            Some(bytes) => {
                                let local_x = (i64::from(x) - left) as usize;
                                half::unorm8_value(bytes[local_y * TILE + local_x])
                            }
                        };
                        if coverage == 0.0 {
                            continue;
                        }
                        let scale = coverage * self.opacity;
                        let color = self.color(x, y);
                        over(
                            &mut target[line + (x - rows.x) as usize],
                            [
                                color[0] * scale,
                                color[1] * scale,
                                color[2] * scale,
                                color[3] * scale,
                            ],
                        );
                    }
                }
            }
        }
        true
    }
}

/// The canvas pixels `coverage`'s non-empty tiles cover.
fn drawn_area(coverage: &TileCoverage, size: RenderSize) -> PixelBox {
    let tiles_x = coverage.size.0 as usize;
    let tile = i32::try_from(TILE).unwrap_or(16);
    let mut area = PixelBox::EMPTY;
    for (index, state) in coverage.tiles.iter().enumerate() {
        if *state == TileState::Empty {
            continue;
        }
        let (tx, ty) = (
            coverage.origin.0 + i32::try_from(index % tiles_x).unwrap_or(0),
            coverage.origin.1 + i32::try_from(index / tiles_x).unwrap_or(0),
        );
        let rect = IRect::new(tx * tile, ty * tile, (tx + 1) * tile, (ty + 1) * tile);
        area = area.union(scissor(rect, size));
    }
    area
}

/// `rect` within a picture of `size`: empty when nothing of it is inside.
fn scissor(rect: IRect, size: RenderSize) -> PixelBox {
    let canvas = IRect::new(
        0,
        0,
        i32::try_from(size.width).unwrap_or(i32::MAX),
        i32::try_from(size.height).unwrap_or(i32::MAX),
    );
    let inside = rect.intersect(canvas);
    if inside.is_empty() {
        return PixelBox::EMPTY;
    }
    PixelBox {
        x: u32::try_from(inside.x0).unwrap_or(0),
        y: u32::try_from(inside.y0).unwrap_or(0),
        width: inside.width(),
        height: inside.height(),
    }
}
