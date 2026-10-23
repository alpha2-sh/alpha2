//! Coverage of a path: per pixel, the average winding number over the
//! pixel's area, mapped by the fill rule, in 16 by 16 pixel tiles.
//!
//! Lines are accumulated only in the columns an edge touches, one strip of
//! tile rows at a time ([`rasterize`]); a tile no edge reaches takes the
//! winding of the run it lies in, without per-pixel work, and is [`Full`]
//! or [`Empty`]. The prefix sums add the same non-zero contributions in the
//! same order as accumulating every pixel would ([`dense::rasterize_dense`],
//! kept as the reference), so both give the same bytes.
//!
//! Everything is scalar code in a fixed order: `f64` geometry, `f32`
//! accumulation, no fused multiply-add. Coverage is the same bytes on every
//! target.
//!
//! [`Full`]: TileState::Full
//! [`Empty`]: TileState::Empty

mod accumulate;
mod cache;
pub mod dense;
mod flatten;

pub use cache::{CoverageCache, coverage_key};

use crate::IRect;
use accumulate::{accumulate_line, clip_line};
use alpha2_model::FillRule;
use kurbo::{BezPath, Point};

/// Tile edge, in pixels.
pub const TILE: usize = 16;

/// Bytes of one tile's coverage.
pub const TILE_BYTES: usize = TILE * TILE;

/// What one tile of coverage holds.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub enum TileState {
    /// No coverage anywhere in it.
    Empty,
    /// Full coverage everywhere in it.
    Full,
    /// Coverage that varies: the index of its bytes in
    /// [`TileCoverage::partial`].
    Partial(u32),
}

/// A path's coverage over a rectangle of whole tiles.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TileCoverage {
    /// The first tile's position, in tiles: pixel `(16 x, 16 y)`.
    pub origin: (i32, i32),
    /// Tiles across and down.
    pub size: (u32, u32),
    /// Each tile, row by row.
    pub tiles: Vec<TileState>,
    /// The bytes of each partial tile, row by row within it.
    pub partial: Vec<[u8; TILE_BYTES]>,
}

impl TileCoverage {
    /// The coverage of pixel `(x, y)`, zero outside the tiles.
    #[must_use]
    pub fn at(&self, x: i32, y: i32) -> u8 {
        let tile = i32::try_from(TILE).unwrap_or(16);
        let (tx, ty) = (
            x.div_euclid(tile) - self.origin.0,
            y.div_euclid(tile) - self.origin.1,
        );
        let (Ok(tx), Ok(ty)) = (u32::try_from(tx), u32::try_from(ty)) else {
            return 0;
        };
        if tx >= self.size.0 || ty >= self.size.1 {
            return 0;
        }
        match self.tiles[(ty * self.size.0 + tx) as usize] {
            TileState::Empty => 0,
            TileState::Full => 255,
            TileState::Partial(index) => {
                let (cx, cy) = (x.rem_euclid(tile), y.rem_euclid(tile));
                self.partial[index as usize][usize::try_from(cy * tile + cx).unwrap_or(0)]
            }
        }
    }

    /// The pixel rectangle the tiles cover.
    #[must_use]
    pub fn rect(&self) -> IRect {
        let tile = i32::try_from(TILE).unwrap_or(16);
        let width = i32::try_from(self.size.0).unwrap_or(0);
        let height = i32::try_from(self.size.1).unwrap_or(0);
        IRect::new(
            self.origin.0 * tile,
            self.origin.1 * tile,
            (self.origin.0 + width) * tile,
            (self.origin.1 + height) * tile,
        )
    }

    /// The heap it holds, as a cache counts it.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.tiles.len() * size_of::<TileState>() + self.partial.len() * TILE_BYTES
    }

    /// Counts of empty, full, and partial tiles.
    #[must_use]
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut counts = (0, 0, 0);
        for tile in &self.tiles {
            match tile {
                TileState::Empty => counts.0 += 1,
                TileState::Full => counts.1 += 1,
                TileState::Partial(_) => counts.2 += 1,
            }
        }
        counts
    }
}

/// `clip` grown outward to whole tiles.
#[must_use]
pub fn tile_aligned(clip: IRect) -> IRect {
    let tile = i32::try_from(TILE).unwrap_or(16);
    if clip.is_empty() {
        return IRect::default();
    }
    IRect::new(
        clip.x0.div_euclid(tile) * tile,
        clip.y0.div_euclid(tile) * tile,
        (clip.x1 + tile - 1).div_euclid(tile) * tile,
        (clip.y1 + tile - 1).div_euclid(tile) * tile,
    )
}

/// The coverage byte of a pixel whose average winding is `winding`.
/// Even-odd is a triangle wave of period 2, found with a truncating
/// conversion rather than `floor` (a library call on baseline x86-64).
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
pub(crate) fn coverage_byte(rule: FillRule, winding: f32) -> u8 {
    let coverage = match rule {
        FillRule::Nonzero => winding.abs().min(1.0),
        FillRule::Evenodd => {
            let half = winding.abs() * 0.5;
            let turn = half - (half as i32) as f32;
            (1.0 - (2.0 * turn - 1.0).abs()).min(1.0)
        }
    };
    (coverage * 255.0 + 0.5) as u8
}

/// A run of one pixel row: constant between the cells edges touched, per
/// pixel within them.
#[derive(Clone, Copy)]
enum Run {
    Const { x0: usize, x1: usize, value: u8 },
    Pixels { x0: usize, x1: usize, offset: usize },
}

impl Run {
    const fn x1(self) -> usize {
        match self {
            Self::Const { x1, .. } | Self::Pixels { x1, .. } => x1,
        }
    }
}

/// Buffers reused from one rasterization to the next.
#[derive(Debug, Default)]
pub struct RasterScratch {
    lines: Vec<[f64; 4]>,
    bins: Vec<Vec<u32>>,
    acc: Vec<f32>,
    touched: Vec<Vec<(usize, usize)>>,
    runs: Vec<Vec<RunSlot>>,
    pixels: Vec<u8>,
    cursor: Vec<usize>,
}

/// A [`Run`] as the scratch stores it: kind (0 constant, 1 pixels), `x0`,
/// `x1`, and the value or offset.
#[derive(Clone, Copy, Debug)]
struct RunSlot(u8, usize, usize, usize);

impl From<Run> for RunSlot {
    fn from(run: Run) -> Self {
        match run {
            Run::Const { x0, x1, value } => Self(0, x0, x1, usize::from(value)),
            Run::Pixels { x0, x1, offset } => Self(1, x0, x1, offset),
        }
    }
}

impl From<RunSlot> for Run {
    #[allow(clippy::cast_possible_truncation)]
    fn from(slot: RunSlot) -> Self {
        if slot.0 == 0 {
            Self::Const {
                x0: slot.1,
                x1: slot.2,
                value: slot.3 as u8,
            }
        } else {
            Self::Pixels {
                x0: slot.1,
                x1: slot.2,
                offset: slot.3,
            }
        }
    }
}

/// The coverage of `path` under `rule` within `clip` (device pixels, grown
/// to whole tiles): per pixel, the average winding number over its area,
/// mapped by the rule. Nonzero is `min(|w|, 1)`; even-odd a triangle wave
/// of period 2 in `|w|`. Only pixels where three or more windings cross
/// within the pixel are approximate.
#[must_use]
// One pass over strips, rows, and tiles; split up, its state would cross function boundaries.
#[allow(clippy::too_many_lines)]
pub fn rasterize(
    path: &BezPath,
    rule: FillRule,
    clip: IRect,
    scratch: &mut RasterScratch,
) -> TileCoverage {
    let clip = tile_aligned(clip);
    let tile = i32::try_from(TILE).unwrap_or(16);
    let (w, h) = (clip.width() as usize, clip.height() as usize);
    let tiles_x = w / TILE;
    let tiles_y = h / TILE;
    let mut out = TileCoverage {
        origin: (clip.x0.div_euclid(tile), clip.y0.div_euclid(tile)),
        size: (
            u32::try_from(tiles_x).unwrap_or(0),
            u32::try_from(tiles_y).unwrap_or(0),
        ),
        tiles: Vec::with_capacity(tiles_x * tiles_y),
        partial: Vec::new(),
    };
    if w == 0 || h == 0 {
        return out;
    }
    flatten::flatten(
        path,
        Point::new(f64::from(clip.x0), f64::from(clip.y0)),
        &mut scratch.lines,
    );
    #[allow(clippy::cast_precision_loss)]
    let (wf, hf) = (w as f64, h as f64);
    // Lines by the strips (tile rows) they reach.
    scratch.bins.resize_with(tiles_y, Vec::new);
    for bin in &mut scratch.bins[..tiles_y] {
        bin.clear();
    }
    for (index, line) in scratch.lines.iter().enumerate() {
        let (lo, hi) = (line[1].min(line[3]).max(0.0), line[1].max(line[3]).min(hf));
        if hi <= lo {
            continue;
        }
        // Both within the clip's height here.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (first, last) = (
            (lo as usize) / TILE,
            (hi.ceil() as usize).div_ceil(TILE).min(tiles_y),
        );
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        for bin in &mut scratch.bins[first..last] {
            bin.push(index);
        }
    }
    let stride = w + 2;
    scratch.acc.clear();
    scratch.acc.resize(stride * TILE, 0.0);
    scratch.touched.resize_with(TILE, Vec::new);
    scratch.runs.resize_with(TILE, Vec::new);
    scratch.cursor.resize(TILE, 0);
    #[allow(clippy::cast_precision_loss)]
    let wf32 = w as f32;
    for strip in 0..tiles_y {
        let r0 = strip * TILE;
        let r1 = r0 + TILE;
        for list in &mut scratch.touched[..TILE] {
            list.clear();
        }
        {
            let RasterScratch {
                lines,
                bins,
                acc,
                touched,
                ..
            } = &mut *scratch;
            for &index in &bins[strip] {
                clip_line(lines[index as usize], wf, hf, |p0, p1| {
                    accumulate_line(acc, stride, r0, (r0, r1), wf32, p0, p1, |y, first, last| {
                        touched[y - r0].push((first, last));
                    });
                });
            }
        }
        // Each row as runs: constant between the touched ranges, per pixel within them.
        scratch.pixels.clear();
        for r in 0..TILE {
            let row = r * stride;
            let RasterScratch {
                acc,
                touched,
                runs,
                pixels,
                ..
            } = &mut *scratch;
            let ranges = &mut touched[r];
            ranges.sort_unstable();
            let runs = &mut runs[r];
            runs.clear();
            let (mut winding, mut cursor) = (0.0f32, 0usize);
            let mut index = 0;
            while index < ranges.len() {
                let (lo, mut hi) = ranges[index];
                index += 1;
                while index < ranges.len() && ranges[index].0 <= hi + 1 {
                    hi = hi.max(ranges[index].1);
                    index += 1;
                }
                if lo > cursor && cursor < w {
                    runs.push(
                        Run::Const {
                            x0: cursor,
                            x1: lo.min(w),
                            value: coverage_byte(rule, winding),
                        }
                        .into(),
                    );
                }
                let offset = pixels.len();
                for x in lo..=hi {
                    winding += acc[row + x];
                    acc[row + x] = 0.0;
                    if x < w {
                        pixels.push(coverage_byte(rule, winding));
                    }
                }
                if lo < w {
                    runs.push(
                        Run::Pixels {
                            x0: lo,
                            x1: (hi + 1).min(w),
                            offset,
                        }
                        .into(),
                    );
                }
                cursor = hi + 1;
            }
            if cursor < w {
                runs.push(
                    Run::Const {
                        x0: cursor,
                        x1: w,
                        value: coverage_byte(rule, winding),
                    }
                    .into(),
                );
            }
        }
        // Tiles: uniform when every row lies in one constant run across them.
        for cursor in &mut scratch.cursor[..TILE] {
            *cursor = 0;
        }
        for tx in 0..tiles_x {
            let (cx0, cx1) = (tx * TILE, tx * TILE + TILE);
            let (mut all_full, mut all_empty) = (true, true);
            for r in 0..TILE {
                let runs = &scratch.runs[r];
                let cursor = &mut scratch.cursor[r];
                while *cursor < runs.len() && Run::from(runs[*cursor]).x1() <= cx0 {
                    *cursor += 1;
                }
                match runs.get(*cursor).map(|slot| Run::from(*slot)) {
                    Some(Run::Const { x0, x1, value }) if x0 <= cx0 && x1 >= cx1 => {
                        all_full &= value == 255;
                        all_empty &= value == 0;
                    }
                    _ => {
                        all_full = false;
                        all_empty = false;
                    }
                }
            }
            let state = if all_full {
                TileState::Full
            } else if all_empty {
                TileState::Empty
            } else {
                let mut bytes = [0u8; TILE_BYTES];
                for r in 0..TILE {
                    let runs = &scratch.runs[r];
                    let mut at = scratch.cursor[r];
                    while at < runs.len() {
                        let run = Run::from(runs[at]);
                        let (x0, x1) = match run {
                            Run::Const { x0, x1, .. } | Run::Pixels { x0, x1, .. } => (x0, x1),
                        };
                        if x0 >= cx1 {
                            break;
                        }
                        for x in x0.max(cx0)..x1.min(cx1) {
                            bytes[r * TILE + (x - cx0)] = match run {
                                Run::Const { value, .. } => value,
                                Run::Pixels { x0, offset, .. } => scratch.pixels[offset + (x - x0)],
                            };
                        }
                        at += 1;
                    }
                }
                // An edge may pass through a tile and leave it uniform.
                if bytes.iter().all(|byte| *byte == 255) {
                    TileState::Full
                } else if bytes.iter().all(|byte| *byte == 0) {
                    TileState::Empty
                } else {
                    out.partial.push(bytes);
                    TileState::Partial(u32::try_from(out.partial.len() - 1).unwrap_or(u32::MAX))
                }
            };
            out.tiles.push(state);
        }
    }
    out
}

/// The flattened lines [`rasterize`] accumulates for `path` in `clip`,
/// relative to the tile-aligned clip's corner.
#[must_use]
pub fn flattened(path: &BezPath, clip: IRect) -> Vec<[f64; 4]> {
    let clip = tile_aligned(clip);
    let mut lines = Vec::new();
    flatten::flatten(
        path,
        Point::new(f64::from(clip.x0), f64::from(clip.y0)),
        &mut lines,
    );
    lines
}

/// The winding number of `path` around `point`, from its flattened lines:
/// what a fill rule decides inside from, for hit tests.
#[must_use]
pub fn winding_at(path: &BezPath, point: Point) -> i32 {
    let mut lines = Vec::new();
    flatten::flatten(path, Point::ZERO, &mut lines);
    let mut winding = 0;
    for [x0, y0, x1, y1] in lines {
        // Upward crossings count one way, downward the other; a vertex on the ray counts once.
        if (y0 <= point.y) != (y1 <= point.y) {
            let x = x0 + (point.y - y0) * (x1 - x0) / (y1 - y0);
            if x > point.x {
                winding += if y1 > y0 { 1 } else { -1 };
            }
        }
    }
    winding
}

#[cfg(test)]
mod tests;
