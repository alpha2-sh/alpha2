use crate::fit::Similarity;
use crate::image::{Pyramid, scale_of};

const FIT_SAMPLES: f64 = 1024.0;
const SEARCH_SAMPLES: f64 = 256.0;
const MIN_VISIBLE_SHARE: f64 = 0.5;
const FLAT_VARIANCE: f64 = 1.0;
const SEARCH_STEPS_PER_RADIUS: f64 = 24.0;
const FINEST_STEP_PIXELS: f64 = 0.25;
const CLIMBS_PER_STEP: usize = 8;
const REFINE_ITERATIONS: usize = 12;
const REFINED_PIXELS: f64 = 0.005;
const MAX_REFINED_SCALE_CHANGE: f64 = 1.25;

pub struct Template {
    reference: Pyramid,
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

struct Probe {
    points: Vec<(f64, f64)>,
    values: Vec<f32>,
    spacing: f64,
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "sample counts and pixel positions far inside every float and integer type"
)]
fn level_for(pyramid: &Pyramid, pixels: f64) -> usize {
    pyramid.level_for(pixels as f32)
}

impl Template {
    pub fn new(reference: Pyramid, left: f64, top: f64, width: f64, height: f64) -> Self {
        Self {
            reference,
            left,
            top,
            width,
            height,
        }
    }

    pub fn centre(&self) -> (f64, f64) {
        (self.left + 0.5 * self.width, self.top + 0.5 * self.height)
    }

    pub fn extent(&self) -> f64 {
        self.width.max(self.height)
    }

    pub fn holds(&self, (x, y): (f64, f64)) -> bool {
        (self.left..=self.left + self.width).contains(&x)
            && (self.top..=self.top + self.height).contains(&y)
    }

    pub fn corners(&self) -> [(f64, f64); 4] {
        let (right, bottom) = (self.left + self.width, self.top + self.height);
        [
            (self.left, self.top),
            (right, self.top),
            (self.left, bottom),
            (right, bottom),
        ]
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "sample counts and pixel positions far inside every float and integer type"
    )]
    fn probe(&self, samples: f64, blur: f64) -> Probe {
        let columns = (samples * self.width / self.height)
            .sqrt()
            .round()
            .clamp(4.0, 256.0) as usize;
        let rows = (samples / columns as f64).round().clamp(4.0, 256.0) as usize;
        let spacing = (self.width / columns as f64).max(self.height / rows as f64);
        let level = level_for(&self.reference, spacing).max(level_for(&self.reference, blur));
        let plane = &self.reference.levels[level];
        let scale = f64::from(scale_of(level));
        let mut points = Vec::with_capacity(columns * rows);
        let mut values = Vec::with_capacity(columns * rows);
        for row in 0..rows {
            let y = self.top + (row as f64 + 0.5) * self.height / rows as f64;
            for column in 0..columns {
                let x = self.left + (column as f64 + 0.5) * self.width / columns as f64;
                points.push((x, y));
                values.push(plane.sample((x / scale) as f32, (y / scale) as f32));
            }
        }
        Probe {
            points,
            values,
            spacing,
        }
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "sample counts and pixel positions far inside every float and integer type"
    )]
    fn correlate(probe: &Probe, picture: &Pyramid, motion: &Similarity, blur: f64) -> f32 {
        let level =
            level_for(picture, probe.spacing * motion.scale()).max(level_for(picture, blur));
        let plane = &picture.levels[level];
        let scale = f64::from(scale_of(level));
        let base = picture.base();
        let (mut n, mut st, mut sc, mut stt, mut scc, mut stc) = (0.0, 0.0, 0.0, 0.0, 0.0, 0.0);
        for (point, template) in probe.points.iter().zip(&probe.values) {
            let (x, y) = motion.apply(*point);
            if !base.contains(x as f32, y as f32) {
                continue;
            }
            let template = f64::from(*template);
            let seen = f64::from(plane.sample((x / scale) as f32, (y / scale) as f32));
            n += 1.0;
            st += template;
            sc += seen;
            stt += template * template;
            scc += seen * seen;
            stc += template * seen;
        }
        if n < MIN_VISIBLE_SHARE * probe.points.len() as f64 {
            return 0.0;
        }
        let template_variance = stt / n - (st / n) * (st / n);
        let seen_variance = scc / n - (sc / n) * (sc / n);
        if template_variance < FLAT_VARIANCE || seen_variance < FLAT_VARIANCE {
            return 0.0;
        }
        let covariance = stc / n - (st / n) * (sc / n);
        (covariance / (template_variance * seen_variance).sqrt()).clamp(0.0, 1.0) as f32
    }

    pub fn fit(&self, picture: &Pyramid, motion: &Similarity) -> f32 {
        Self::correlate(&self.probe(FIT_SAMPLES, 0.0), picture, motion, 0.0)
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "sample counts and pixel positions far inside every float and integer type"
    )]
    pub fn search(&self, picture: &Pyramid, around: &Similarity, radius: f64) -> (Similarity, f32) {
        let fine = self.probe(FIT_SAMPLES, 0.0);
        let step = (radius / SEARCH_STEPS_PER_RADIUS)
            .max(fine.spacing * around.scale())
            .max(1.0);
        let coarse = self.probe(SEARCH_SAMPLES, step / around.scale());
        let reach = (radius / step).ceil() as i32;
        let mut best = (*around, -1.0_f32);
        for row in -reach..=reach {
            for column in -reach..=reach {
                let candidate = around.shifted(f64::from(column) * step, f64::from(row) * step);
                let score = Self::correlate(&coarse, picture, &candidate, step);
                if score > best.1 {
                    best = (candidate, score);
                }
            }
        }
        let mut motion = best.0;
        let mut score = Self::correlate(&fine, picture, &motion, 0.0);
        let mut stride = 0.5 * step;
        while stride >= FINEST_STEP_PIXELS {
            for _ in 0..CLIMBS_PER_STEP {
                let mut moved = false;
                for (dx, dy) in [
                    (-1.0, -1.0),
                    (0.0, -1.0),
                    (1.0, -1.0),
                    (-1.0, 0.0),
                    (1.0, 0.0),
                    (-1.0, 1.0),
                    (0.0, 1.0),
                    (1.0, 1.0),
                ] {
                    let candidate = motion.shifted(dx * stride, dy * stride);
                    let candidate_score = Self::correlate(&fine, picture, &candidate, 0.0);
                    if candidate_score > score {
                        (motion, score) = (candidate, candidate_score);
                        moved = true;
                    }
                }
                if !moved {
                    break;
                }
            }
            stride *= 0.5;
        }
        (motion, score)
    }

    /// Gauss-Newton on the similarity's four parameters with Tukey's biweight.
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss,
        reason = "sample counts and pixel positions far inside every float and integer type"
    )]
    pub fn refine(&self, picture: &Pyramid, motion: &Similarity) -> Similarity {
        let probe = self.probe(FIT_SAMPLES, 0.0);
        let level = level_for(picture, probe.spacing * motion.scale());
        let plane = &picture.levels[level];
        let scale = f64::from(scale_of(level));
        let base = picture.base();
        let (cx, cy) = self.centre();
        let (mut a, mut b) = (motion.a, motion.b);
        let (mut ux, mut uy) = motion.apply((cx, cy));
        let mut settled = false;
        for _ in 0..REFINE_ITERATIONS {
            let mut seen = Vec::with_capacity(probe.points.len());
            for (point, template) in probe.points.iter().zip(&probe.values) {
                let (qx, qy) = (point.0 - cx, point.1 - cy);
                let (x, y) = (a * qx - b * qy + ux, b * qx + a * qy + uy);
                if !base.contains(x as f32, y as f32) {
                    continue;
                }
                let (lx, ly) = ((x / scale) as f32, (y / scale) as f32);
                let value = f64::from(plane.sample(lx, ly));
                let gx = f64::from(plane.sample(lx + 1.0, ly) - plane.sample(lx - 1.0, ly))
                    / (2.0 * scale);
                let gy = f64::from(plane.sample(lx, ly + 1.0) - plane.sample(lx, ly - 1.0))
                    / (2.0 * scale);
                seen.push((qx, qy, f64::from(*template), value, gx, gy));
            }
            if (seen.len() as f64) < MIN_VISIBLE_SHARE * probe.points.len() as f64 {
                return *motion;
            }
            let n = seen.len() as f64;
            let mean_t = seen.iter().map(|s| s.2).sum::<f64>() / n;
            let mean_c = seen.iter().map(|s| s.3).sum::<f64>() / n;
            let var_t = seen
                .iter()
                .map(|s| (s.2 - mean_t) * (s.2 - mean_t))
                .sum::<f64>()
                / n;
            let var_c = seen
                .iter()
                .map(|s| (s.3 - mean_c) * (s.3 - mean_c))
                .sum::<f64>()
                / n;
            if var_t < FLAT_VARIANCE || var_c < FLAT_VARIANCE {
                return *motion;
            }
            let gain = (var_c / var_t).sqrt();
            let residuals = seen
                .iter()
                .map(|s| (s.3 - mean_c) - gain * (s.2 - mean_t))
                .collect::<Vec<_>>();
            let cutoff = tukey_cutoff(&residuals);
            let mut hessian = [[0.0_f64; 4]; 4];
            let mut gradient = [0.0_f64; 4];
            for ((qx, qy, _, _, gx, gy), residual) in seen.into_iter().zip(residuals) {
                let weight = tukey_weight(residual, cutoff);
                if weight == 0.0 {
                    continue;
                }
                let jacobian = [gx * qx + gy * qy, gy * qx - gx * qy, gx, gy];
                for row in 0..4 {
                    gradient[row] += weight * jacobian[row] * residual;
                    for column in 0..4 {
                        hessian[row][column] += weight * jacobian[row] * jacobian[column];
                    }
                }
            }
            let Some(step) = gaussian_elimination(hessian, gradient.map(|value| -value)) else {
                return *motion;
            };
            a += step[0];
            b += step[1];
            ux += step[2];
            uy += step[3];
            let extent = self.extent();
            if step[2]
                .abs()
                .max(step[3].abs())
                .max(step[0].abs().max(step[1].abs()) * extent)
                < REFINED_PIXELS
            {
                settled = true;
                break;
            }
        }
        let refined = Similarity {
            a,
            b,
            tx: ux - (a * cx - b * cy),
            ty: uy - (b * cx + a * cy),
        };
        let change = refined.scale() / motion.scale();
        let plausible = refined.tx.is_finite()
            && refined.ty.is_finite()
            && (1.0 / MAX_REFINED_SCALE_CHANGE..=MAX_REFINED_SCALE_CHANGE).contains(&change);
        if settled && plausible {
            refined
        } else {
            *motion
        }
    }
}

/// Tukey's 4.685 robust standard deviations keep 95% efficiency on Gaussian residuals; the median
/// absolute deviation times 1.4826 estimates the standard deviation.
fn tukey_cutoff(residuals: &[f64]) -> f64 {
    let mut magnitudes = residuals
        .iter()
        .map(|value| value.abs())
        .collect::<Vec<_>>();
    magnitudes.sort_by(f64::total_cmp);
    let median = magnitudes.get(magnitudes.len() / 2).copied().unwrap_or(0.0);
    (4.685 * 1.4826 * median).max(1.0)
}

fn tukey_weight(residual: f64, cutoff: f64) -> f64 {
    let share = residual / cutoff;
    if share.abs() >= 1.0 {
        0.0
    } else {
        (1.0 - share * share) * (1.0 - share * share)
    }
}

fn gaussian_elimination(mut matrix: [[f64; 4]; 4], mut vector: [f64; 4]) -> Option<[f64; 4]> {
    for column in 0..4 {
        let pivot = (column..4)
            .max_by(|&i, &j| matrix[i][column].abs().total_cmp(&matrix[j][column].abs()))?;
        if matrix[pivot][column].abs() < 1e-12 {
            return None;
        }
        matrix.swap(column, pivot);
        vector.swap(column, pivot);
        let pivot_row = matrix[column];
        for row in column + 1..4 {
            let factor = matrix[row][column] / pivot_row[column];
            for (cell, above) in matrix[row].iter_mut().zip(pivot_row).skip(column) {
                *cell -= factor * above;
            }
            vector[row] -= factor * vector[column];
        }
    }
    let mut x = [0.0; 4];
    for row in (0..4).rev() {
        let tail = (row + 1..4).map(|k| matrix[row][k] * x[k]).sum::<f64>();
        x[row] = (vector[row] - tail) / matrix[row][row];
    }
    x.iter().all(|value| value.is_finite()).then_some(x)
}
