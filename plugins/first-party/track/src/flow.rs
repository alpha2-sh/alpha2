use crate::corners::{MIN_EIGEN_PER_PIXEL, min_eigenvalue};
use crate::image::{Pyramid, scale_of};

pub const WINDOW_RADIUS: usize = 7;
const WINDOW_SIDE: usize = 2 * WINDOW_RADIUS + 1;
const WINDOW_PIXELS: usize = WINDOW_SIDE * WINDOW_SIDE;
const ITERATIONS: usize = 12;
const CONVERGED_PIXELS: f32 = 0.01;
const SINGULAR_DETERMINANT: f32 = 1e-6;

#[expect(clippy::cast_precision_loss, reason = "window offsets of a few pixels")]
fn window_offset(index: usize) -> f32 {
    index as f32 - WINDOW_RADIUS as f32
}

/// Pyramidal Lucas-Kanade as Bouguet describes it.
pub fn track(from: &Pyramid, to: &Pyramid, point: (f32, f32)) -> Option<(f32, f32)> {
    let levels = from.levels.len().min(to.levels.len());
    let mut guess = (0.0_f32, 0.0_f32);
    for level in (0..levels).rev() {
        let scale = scale_of(level);
        let source = &from.levels[level];
        let target = &to.levels[level];
        let (x, y) = (point.0 / scale, point.1 / scale);
        let mut values = [0.0_f32; WINDOW_PIXELS];
        let mut gradients = [(0.0_f32, 0.0_f32); WINDOW_PIXELS];
        let (mut xx, mut xy, mut yy) = (0.0_f32, 0.0_f32, 0.0_f32);
        for row in 0..WINDOW_SIDE {
            for column in 0..WINDOW_SIDE {
                let (sx, sy) = (x + window_offset(column), y + window_offset(row));
                let gx = 0.5 * (source.sample(sx + 1.0, sy) - source.sample(sx - 1.0, sy));
                let gy = 0.5 * (source.sample(sx, sy + 1.0) - source.sample(sx, sy - 1.0));
                let at = row * WINDOW_SIDE + column;
                values[at] = source.sample(sx, sy);
                gradients[at] = (gx, gy);
                xx += gx * gx;
                xy += gx * gy;
                yy += gy * gy;
            }
        }
        let determinant = xx * yy - xy * xy;
        #[expect(clippy::cast_precision_loss, reason = "window offsets of a few pixels")]
        let textured = min_eigenvalue(xx, xy, yy) / WINDOW_PIXELS as f32 >= MIN_EIGEN_PER_PIXEL;
        if level == 0 && !textured {
            return None;
        }
        let mut step = (0.0_f32, 0.0_f32);
        if determinant > SINGULAR_DETERMINANT * (xx + yy) * (xx + yy) {
            for _ in 0..ITERATIONS {
                let (mut bx, mut by) = (0.0_f32, 0.0_f32);
                for row in 0..WINDOW_SIDE {
                    for column in 0..WINDOW_SIDE {
                        let at = row * WINDOW_SIDE + column;
                        let moved = target.sample(
                            x + window_offset(column) + guess.0 + step.0,
                            y + window_offset(row) + guess.1 + step.1,
                        );
                        let difference = values[at] - moved;
                        bx += difference * gradients[at].0;
                        by += difference * gradients[at].1;
                    }
                }
                let dx = (yy * bx - xy * by) / determinant;
                let dy = (xx * by - xy * bx) / determinant;
                step = (step.0 + dx, step.1 + dy);
                if dx * dx + dy * dy < CONVERGED_PIXELS * CONVERGED_PIXELS {
                    break;
                }
            }
        } else if level == 0 {
            return None;
        }
        guess = (guess.0 + step.0, guess.1 + step.1);
        if level > 0 {
            guess = (2.0 * guess.0, 2.0 * guess.1);
        }
    }
    let found = (point.0 + guess.0, point.1 + guess.1);
    (found.0.is_finite() && found.1.is_finite() && to.base().contains(found.0, found.1))
        .then_some(found)
}
