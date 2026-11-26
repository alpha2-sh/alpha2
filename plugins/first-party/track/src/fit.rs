pub const MIN_INLIERS: usize = 6;
const INLIER_PIXELS: f64 = 2.0;
const ITERATIONS: usize = 128;
const SHORTEST_BASE_PIXELS: f64 = 2.0;
const SEED: u64 = 0x7472_6163_6b65_7231;

pub type Pair = ((f64, f64), (f64, f64));

/// `p -> [[a, -b], [b, a]] p + (tx, ty)`, in pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Similarity {
    pub a: f64,
    pub b: f64,
    pub tx: f64,
    pub ty: f64,
}

impl Similarity {
    pub const IDENTITY: Self = Self {
        a: 1.0,
        b: 0.0,
        tx: 0.0,
        ty: 0.0,
    };

    pub fn apply(&self, (x, y): (f64, f64)) -> (f64, f64) {
        (
            self.a * x - self.b * y + self.tx,
            self.b * x + self.a * y + self.ty,
        )
    }

    pub fn scale(&self) -> f64 {
        libm::hypot(self.a, self.b)
    }

    /// Clockwise in pictures, whose `y` grows downward, in (-180, 180].
    pub fn degrees(&self) -> f64 {
        libm::atan2(self.b, self.a).to_degrees()
    }

    pub fn inverse(&self) -> Self {
        let norm = self.a * self.a + self.b * self.b;
        let (a, b) = (self.a / norm, -self.b / norm);
        Self {
            a,
            b,
            tx: -(a * self.tx - b * self.ty),
            ty: -(b * self.tx + a * self.ty),
        }
    }

    pub fn shifted(&self, dx: f64, dy: f64) -> Self {
        Self {
            tx: self.tx + dx,
            ty: self.ty + dy,
            ..*self
        }
    }

    fn through(from: [(f64, f64); 2], to: [(f64, f64); 2]) -> Option<Self> {
        let (fx, fy) = (from[1].0 - from[0].0, from[1].1 - from[0].1);
        let (tx, ty) = (to[1].0 - to[0].0, to[1].1 - to[0].1);
        let norm = fx * fx + fy * fy;
        if norm < SHORTEST_BASE_PIXELS * SHORTEST_BASE_PIXELS {
            return None;
        }
        let a = (fx * tx + fy * ty) / norm;
        let b = (fx * ty - fy * tx) / norm;
        Some(Self {
            a,
            b,
            tx: to[0].0 - (a * from[0].0 - b * from[0].1),
            ty: to[0].1 - (b * from[0].0 + a * from[0].1),
        })
    }

    #[expect(
        clippy::cast_precision_loss,
        reason = "feature counts of at most a few dozen"
    )]
    fn least_squares(pairs: &[Pair], chosen: &[bool]) -> Option<Self> {
        let picked = || pairs.iter().zip(chosen).filter(|(_, in_set)| **in_set);
        let count = picked().count();
        if count < 2 {
            return None;
        }
        let n = count as f64;
        let (mut fx, mut fy, mut tx, mut ty) = (0.0, 0.0, 0.0, 0.0);
        for ((from, to), _) in picked() {
            fx += from.0;
            fy += from.1;
            tx += to.0;
            ty += to.1;
        }
        let (fx, fy, tx, ty) = (fx / n, fy / n, tx / n, ty / n);
        let (mut dot, mut cross, mut norm) = (0.0, 0.0, 0.0);
        for ((from, to), _) in picked() {
            let (ax, ay) = (from.0 - fx, from.1 - fy);
            let (bx, by) = (to.0 - tx, to.1 - ty);
            dot += ax * bx + ay * by;
            cross += ax * by - ay * bx;
            norm += ax * ax + ay * ay;
        }
        if norm < SHORTEST_BASE_PIXELS * SHORTEST_BASE_PIXELS {
            return None;
        }
        let (a, b) = (dot / norm, cross / norm);
        Some(Self {
            a,
            b,
            tx: tx - (a * fx - b * fy),
            ty: ty - (b * fx + a * fy),
        })
    }
}

struct SplitMix64(u64);

impl SplitMix64 {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, count: usize) -> usize {
        (((self.next() >> 32) * count as u64) >> 32) as usize
    }
}

fn inliers(model: &Similarity, pairs: &[Pair]) -> (Vec<bool>, usize, f64) {
    let mut count = 0;
    let mut error = 0.0;
    let marks = pairs
        .iter()
        .map(|(from, to)| {
            let (x, y) = model.apply(*from);
            let squared = (x - to.0) * (x - to.0) + (y - to.1) * (y - to.1);
            let inlier = squared <= INLIER_PIXELS * INLIER_PIXELS;
            if inlier {
                count += 1;
                error += squared;
            }
            inlier
        })
        .collect();
    (marks, count, error)
}

/// RANSAC over pairs of pairs, then least squares on the inliers.
pub fn ransac(pairs: &[Pair], round: u64) -> Option<(Similarity, Vec<bool>)> {
    let count = pairs.len();
    if count < MIN_INLIERS {
        return None;
    }
    let mut sampler = SplitMix64(SEED ^ round.wrapping_mul(0x2545_f491_4f6c_dd1d));
    let mut best: Option<(usize, f64, Similarity)> = None;
    for _ in 0..ITERATIONS {
        let first = sampler.below(count);
        let mut second = sampler.below(count - 1);
        if second >= first {
            second += 1;
        }
        let Some(model) = Similarity::through(
            [pairs[first].0, pairs[second].0],
            [pairs[first].1, pairs[second].1],
        ) else {
            continue;
        };
        let (_, agree, error) = inliers(&model, pairs);
        if best.is_none_or(|(most, least, _)| agree > most || (agree == most && error < least)) {
            best = Some((agree, error, model));
        }
    }
    let (_, _, mut model) = best?;
    let mut marks = inliers(&model, pairs).0;
    for _ in 0..2 {
        model = Similarity::least_squares(pairs, &marks)?;
        marks = inliers(&model, pairs).0;
    }
    (marks.iter().filter(|inlier| **inlier).count() >= MIN_INLIERS).then_some((model, marks))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ransac_recovers_a_similarity_despite_a_third_of_outliers() {
        let truth = Similarity {
            a: 1.1 * libm::cos(0.3),
            b: 1.1 * libm::sin(0.3),
            tx: 12.0,
            ty: -7.0,
        };
        let pairs = (0..30)
            .map(|index| {
                let from = (f64::from(index % 6) * 9.0, f64::from(index / 6) * 11.0);
                let to = if index % 3 == 0 {
                    (f64::from(index) * 3.7, 40.0 - f64::from(index))
                } else {
                    truth.apply(from)
                };
                (from, to)
            })
            .collect::<Vec<_>>();
        let (fit, marks) = ransac(&pairs, 0).expect("a fit");
        for (index, inlier) in marks.iter().enumerate() {
            assert_eq!(*inlier, index % 3 != 0, "pair {index}");
        }
        for (found, expected) in [(fit.a, truth.a), (fit.b, truth.b), (fit.tx, truth.tx)] {
            assert!(
                (found - expected).abs() < 1e-9,
                "{found} against {expected}"
            );
        }
        assert_eq!(ransac(&pairs, 0), ransac(&pairs, 0));
    }

    #[test]
    fn a_similarity_and_its_inverse_cancel() {
        let motion = Similarity {
            a: 0.8,
            b: -0.4,
            tx: 3.0,
            ty: 5.0,
        };
        let (x, y) = motion.inverse().apply(motion.apply((7.0, -2.0)));
        assert!((x - 7.0).abs() < 1e-12 && (y + 2.0).abs() < 1e-12);
    }
}
