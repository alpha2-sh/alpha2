//! Signed-area accumulation of lines into rows of cells, font-rs style.
//!
//! Each line adds, to every cell of every pixel row it crosses, the signed
//! area it leaves to its right within the cell, and to the next cell the
//! rest of its cover. A prefix sum along a row is then each pixel's average
//! winding number over its area.

/// The pieces of a line, relative to a clip `w` by `h` pixels, cut at
/// `x = 0` and `x = w`: a piece left of the clip becomes a vertical line on
/// its left edge (it covers every pixel to its right), one right of it is
/// dropped (it covers none). `draw` gets each piece's ends in `f32`.
// Exact parameters 0 and 1 name the line's own ends, kept bit for bit; the midpoint is the
// reference formula, which `midpoint` would round differently.
#[allow(clippy::float_cmp, clippy::manual_midpoint)]
pub(crate) fn clip_line(
    line: [f64; 4],
    w: f64,
    h: f64,
    mut draw: impl FnMut((f32, f32), (f32, f32)),
) {
    let [x0, y0, x1, y1] = line;
    if (y0 <= 0.0 && y1 <= 0.0) || (y0 >= h && y1 >= h) {
        return;
    }
    let mut cuts = [0.0, 1.0, 1.0, 1.0];
    let mut count = 1;
    for edge in [0.0, w] {
        if (x0 < edge) != (x1 < edge) {
            let t = (edge - x0) / (x1 - x0);
            if t > 0.0 && t < 1.0 {
                cuts[count] = t;
                count += 1;
            }
        }
    }
    cuts[count] = 1.0;
    if count == 3 && cuts[1] > cuts[2] {
        cuts.swap(1, 2);
    }
    let at = |t: f64| -> (f64, f64) {
        if t == 0.0 {
            (x0, y0)
        } else if t == 1.0 {
            (x1, y1)
        } else {
            (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t)
        }
    };
    for piece in 0..count {
        let (a, b) = (at(cuts[piece]), at(cuts[piece + 1]));
        let middle = 0.5 * (a.0 + b.0);
        if middle > w {
            continue;
        }
        let (xa, xb) = if middle < 0.0 {
            (0.0, 0.0)
        } else {
            (a.0.clamp(0.0, w), b.0.clamp(0.0, w))
        };
        // The clip is at most a canvas edge wide and the line has been brought near it, so
        // `f32` keeps the precision coverage needs.
        #[allow(clippy::cast_possible_truncation)]
        draw((xa as f32, a.1 as f32), (xb as f32, b.1 as f32));
    }
}

/// Accumulates one line within `[0, w]` horizontally into the pixel rows
/// `rows.0..rows.1`; row `y` is stored at `(y - base) * stride`. Each row's
/// span is computed from the line's start, not carried from the row above,
/// so a row's contribution does not depend on which rows came before it.
/// `touched(y, first, last)` gets the cells written.
#[allow(
    clippy::too_many_arguments,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::manual_midpoint
)]
pub(crate) fn accumulate_line(
    acc: &mut [f32],
    stride: usize,
    base: usize,
    rows: (usize, usize),
    w: f32,
    p0: (f32, f32),
    p1: (f32, f32),
    mut touched: impl FnMut(usize, usize, usize),
) {
    if (p0.1 - p1.1).abs() <= f32::EPSILON {
        return;
    }
    let (dir, p0, p1) = if p0.1 < p1.1 {
        (1.0f32, p0, p1)
    } else {
        (-1.0f32, p1, p0)
    };
    let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
    let first = if p0.1 < 0.0 { 0 } else { p0.1 as usize };
    let last = p1.1.ceil().max(0.0) as usize;
    for y in first.max(rows.0)..last.min(rows.1) {
        let row = (y - base) * stride;
        let top = (y as f32).max(p0.1);
        let bottom = ((y + 1) as f32).min(p1.1);
        let x = p0.0 + dxdy * (top - p0.1);
        let xnext = p0.0 + dxdy * (bottom - p0.1);
        let d = (bottom - top) * dir;
        let (lo, hi) = if x < xnext { (x, xnext) } else { (xnext, x) };
        let (x0, x1) = (lo.clamp(0.0, w), hi.clamp(0.0, w));
        let x0floor = x0.floor();
        let x0i = x0floor as usize;
        let x1ceil = x1.ceil();
        let x1i = x1ceil as usize;
        if x1i <= x0i + 1 {
            let xmf = 0.5 * (x0 + x1) - x0floor;
            acc[row + x0i] += d - d * xmf;
            acc[row + x0i + 1] += d * xmf;
            touched(y, x0i, x0i + 1);
        } else {
            let s = 1.0 / (x1 - x0);
            let x0f = x0 - x0floor;
            let a0 = 0.5 * s * (1.0 - x0f) * (1.0 - x0f);
            let x1f = x1 - x1ceil + 1.0;
            let am = 0.5 * s * x1f * x1f;
            acc[row + x0i] += d * a0;
            if x1i == x0i + 2 {
                acc[row + x0i + 1] += d * (1.0 - a0 - am);
            } else {
                let a1 = s * (1.5 - x0f);
                acc[row + x0i + 1] += d * (a1 - a0);
                for xi in x0i + 2..x1i - 1 {
                    acc[row + xi] += d * s;
                }
                let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                acc[row + x1i - 1] += d * (1.0 - a2 - am);
            }
            acc[row + x1i] += d * am;
            touched(y, x0i, x1i);
        }
    }
}
