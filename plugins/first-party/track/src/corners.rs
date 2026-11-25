use crate::image::Plane;

const TENSOR_RADIUS: usize = 2;
const TENSOR_PIXELS: f32 = 25.0;
const QUALITY_OF_BEST: f32 = 0.05;
pub const MIN_EIGEN_PER_PIXEL: f32 = 1.0;

#[derive(Clone, Copy, Debug)]
pub struct Area {
    pub left: usize,
    pub top: usize,
    pub right: usize,
    pub bottom: usize,
}

pub fn min_eigenvalue(xx: f32, xy: f32, yy: f32) -> f32 {
    let half_trace = f32::midpoint(xx, yy);
    let half_gap = 0.5 * (xx - yy);
    half_trace - (half_gap * half_gap + xy * xy).sqrt()
}

/// Shi-Tomasi corners, strongest first.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "pixel indices of a frame at most 640 pixels wide"
)]
pub fn strongest(
    plane: &Plane,
    area: Area,
    inside: impl Fn(f32, f32) -> bool,
    spacing: f32,
    taken: &[(f32, f32)],
    want: usize,
) -> Vec<(f32, f32)> {
    let margin = TENSOR_RADIUS + 1;
    let left = area.left.max(margin);
    let top = area.top.max(margin);
    let right = area.right.min(plane.width.saturating_sub(margin));
    let bottom = area.bottom.min(plane.height.saturating_sub(margin));
    if want == 0 || left >= right || top >= bottom {
        return Vec::new();
    }
    let outer_left = left - TENSOR_RADIUS;
    let outer_top = top - TENSOR_RADIUS;
    let outer_width = right - left + 2 * TENSOR_RADIUS;
    let outer_height = bottom - top + 2 * TENSOR_RADIUS;
    let mut tensors = vec![[0.0_f32; 3]; outer_width * outer_height];
    for row in 0..outer_height {
        let y = outer_top + row;
        for column in 0..outer_width {
            let x = outer_left + column;
            let gx = 0.5 * (plane.at(x + 1, y) - plane.at(x - 1, y));
            let gy = 0.5 * (plane.at(x, y + 1) - plane.at(x, y - 1));
            tensors[row * outer_width + column] = [gx * gx, gx * gy, gy * gy];
        }
    }
    let width = right - left;
    let height = bottom - top;
    let mut across = vec![[0.0_f32; 3]; width * outer_height];
    for row in 0..outer_height {
        for column in 0..width {
            let mut sum = [0.0_f32; 3];
            for tap in 0..=2 * TENSOR_RADIUS {
                let tensor = tensors[row * outer_width + column + tap];
                for (total, part) in sum.iter_mut().zip(tensor) {
                    *total += part;
                }
            }
            across[row * width + column] = sum;
        }
    }
    let mut scores = vec![0.0_f32; width * height];
    let mut best = 0.0_f32;
    for row in 0..height {
        for column in 0..width {
            let mut sum = [0.0_f32; 3];
            for tap in 0..=2 * TENSOR_RADIUS {
                let tensor = across[(row + tap) * width + column];
                for (total, part) in sum.iter_mut().zip(tensor) {
                    *total += part;
                }
            }
            let score = min_eigenvalue(sum[0], sum[1], sum[2]);
            scores[row * width + column] = score;
            best = best.max(score);
        }
    }
    let threshold = (QUALITY_OF_BEST * best).max(MIN_EIGEN_PER_PIXEL * TENSOR_PIXELS);
    let cell = (spacing.max(1.0) as usize).max(1);
    let mut candidates = Vec::new();
    for cell_top in (0..height).step_by(cell) {
        for cell_left in (0..width).step_by(cell) {
            let mut pick: Option<(f32, usize, usize)> = None;
            for row in cell_top..(cell_top + cell).min(height) {
                for column in cell_left..(cell_left + cell).min(width) {
                    let score = scores[row * width + column];
                    let (x, y) = ((left + column) as f32, (top + row) as f32);
                    if score >= threshold
                        && pick.is_none_or(|(best, ..)| score > best)
                        && inside(x, y)
                    {
                        pick = Some((score, column, row));
                    }
                }
            }
            if let Some(found) = pick {
                candidates.push(found);
            }
        }
    }
    candidates.sort_by(|a, b| b.0.total_cmp(&a.0).then((a.2, a.1).cmp(&(b.2, b.1))));
    let apart = spacing * spacing;
    let mut chosen: Vec<(f32, f32)> = Vec::new();
    for (_, column, row) in candidates {
        if chosen.len() >= want {
            break;
        }
        let point = ((left + column) as f32, (top + row) as f32);
        let near = |other: &(f32, f32)| {
            let (dx, dy) = (other.0 - point.0, other.1 - point.1);
            dx * dx + dy * dy < apart
        };
        if !chosen.iter().any(near) && !taken.iter().any(near) {
            chosen.push(point);
        }
    }
    chosen
}
