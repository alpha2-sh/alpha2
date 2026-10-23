//! The reference rasterizer: every pixel of the clip accumulated and summed.
//! [`super::rasterize`] must give its bytes; tests and the fuzz target hold
//! it to that.

use super::accumulate::{accumulate_line, clip_line};
use super::{TILE, TILE_BYTES, TileCoverage, TileState, coverage_byte, flatten, tile_aligned};
use crate::IRect;
use alpha2_model::FillRule;
use kurbo::{BezPath, Point};

/// The coverage of `path` under `rule` in `clip` grown to whole tiles, one
/// byte per pixel row by row, and its width and height.
#[must_use]
pub fn coverage_bytes(path: &BezPath, rule: FillRule, clip: IRect) -> (Vec<u8>, usize, usize) {
    let clip = tile_aligned(clip);
    let (w, h) = (clip.width() as usize, clip.height() as usize);
    if w == 0 || h == 0 {
        return (Vec::new(), w, h);
    }
    let mut lines = Vec::new();
    flatten::flatten(
        path,
        Point::new(f64::from(clip.x0), f64::from(clip.y0)),
        &mut lines,
    );
    let stride = w + 2;
    let mut acc = vec![0.0f32; stride * h];
    #[allow(clippy::cast_precision_loss)]
    let (wf, hf, wf32) = (w as f64, h as f64, w as f32);
    for line in lines {
        clip_line(line, wf, hf, |p0, p1| {
            accumulate_line(&mut acc, stride, 0, (0, h), wf32, p0, p1, |_, _, _| {});
        });
    }
    let mut data = vec![0u8; w * h];
    for y in 0..h {
        let mut winding = 0.0f32;
        let row = &acc[y * stride..y * stride + w];
        let out = &mut data[y * w..y * w + w];
        for (value, byte) in row.iter().zip(out.iter_mut()) {
            winding += *value;
            *byte = coverage_byte(rule, winding);
        }
    }
    (data, w, h)
}

/// [`coverage_bytes`] cut into tiles as [`super::rasterize`] cuts them.
#[must_use]
pub fn rasterize_dense(path: &BezPath, rule: FillRule, clip: IRect) -> TileCoverage {
    let aligned = tile_aligned(clip);
    let (data, w, h) = coverage_bytes(path, rule, clip);
    let tile = i32::try_from(TILE).unwrap_or(16);
    let (tiles_x, tiles_y) = (w / TILE, h / TILE);
    let mut out = TileCoverage {
        origin: (aligned.x0.div_euclid(tile), aligned.y0.div_euclid(tile)),
        size: (
            u32::try_from(tiles_x).unwrap_or(0),
            u32::try_from(tiles_y).unwrap_or(0),
        ),
        tiles: Vec::with_capacity(tiles_x * tiles_y),
        partial: Vec::new(),
    };
    for ty in 0..tiles_y {
        for tx in 0..tiles_x {
            let mut bytes = [0u8; TILE_BYTES];
            for r in 0..TILE {
                let start = (ty * TILE + r) * w + tx * TILE;
                bytes[r * TILE..r * TILE + TILE].copy_from_slice(&data[start..start + TILE]);
            }
            let state = if bytes.iter().all(|byte| *byte == 255) {
                TileState::Full
            } else if bytes.iter().all(|byte| *byte == 0) {
                TileState::Empty
            } else {
                out.partial.push(bytes);
                TileState::Partial(u32::try_from(out.partial.len() - 1).unwrap_or(u32::MAX))
            };
            out.tiles.push(state);
        }
    }
    out
}
