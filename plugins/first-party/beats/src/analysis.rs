//! Onset peak picking, tempo estimation and beat tracking over the onset envelope.

use crate::FRAMES_PER_SECOND;

const PEAK_RADIUS_FRAMES: usize = 2;
const MEAN_RADIUS_FRAMES: usize = 4;
const PRIOR_BPM: f64 = 120.0;
const PRIOR_DEVIATION_OCTAVES: f64 = 1.0;
const HALF_PERIOD_CORRELATION_SHARE: f64 = 0.8;
const LAG_STEP_FRAMES: f64 = 0.25;
const TIGHTNESS: f64 = 100.0;
const FIRST_BEAT_LOCAL_SCORE_SHARE: f64 = 0.01;

#[expect(clippy::cast_precision_loss, reason = "counts far below 2^52")]
fn real(count: usize) -> f64 {
    count as f64
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "callers pass small non-negative frame counts"
)]
fn index_at(frames: f64) -> usize {
    frames.max(0.0) as usize
}

pub(crate) fn onsets(envelope: &[f32], rise: f32) -> Vec<usize> {
    let len = envelope.len();
    let mut found = Vec::new();
    for (index, &value) in envelope.iter().enumerate() {
        if value <= 0.0 {
            continue;
        }
        let before = &envelope[index.saturating_sub(PEAK_RADIUS_FRAMES)..index];
        let after = &envelope[index + 1..(index + 1 + PEAK_RADIUS_FRAMES).min(len)];
        if before.iter().any(|&other| other >= value) || after.iter().any(|&other| other > value) {
            continue;
        }
        let around = &envelope
            [index.saturating_sub(MEAN_RADIUS_FRAMES)..(index + 1 + MEAN_RADIUS_FRAMES).min(len)];
        let mean = around.iter().map(|&other| f64::from(other)).sum::<f64>() / real(around.len());
        if f64::from(value) >= mean + f64::from(rise) {
            found.push(index);
        }
    }
    found
}

fn prior(lag: f64) -> f64 {
    let octaves = (60.0 * FRAMES_PER_SECOND / lag / PRIOR_BPM).log2() / PRIOR_DEVIATION_OCTAVES;
    (-0.5 * octaves * octaves).exp()
}

fn binomial_smoothed(values: &[f32]) -> Vec<f64> {
    const TAPS: [f64; 5] = [0.0625, 0.25, 0.375, 0.25, 0.0625];
    (0..values.len())
        .map(|index| {
            TAPS.iter()
                .enumerate()
                .filter_map(|(tap, weight)| {
                    (index + tap)
                        .checked_sub(2)
                        .and_then(|at| values.get(at))
                        .map(|&value| weight * f64::from(value))
                })
                .sum()
        })
        .collect()
}

pub(crate) fn period(flux: &[f32], shortest: f64, longest: f64) -> Option<f64> {
    let len = flux.len();
    if len < 2 {
        return None;
    }
    let smooth = binomial_smoothed(flux);
    let mean = smooth.iter().sum::<f64>() / real(len);
    let centered = smooth.iter().map(|value| value - mean).collect::<Vec<_>>();
    let energy = centered.iter().map(|value| value * value).sum::<f64>() / real(len);
    if energy <= 0.0 {
        return None;
    }
    let top = (index_at(longest.ceil()) + 1).min(len - 1);
    let correlation = (0..=top)
        .map(|lag| {
            let sum = centered[lag..]
                .iter()
                .zip(&centered)
                .map(|(later, earlier)| later * earlier)
                .sum::<f64>();
            sum / real(len - lag) / energy
        })
        .collect::<Vec<_>>();
    let at = |lag: f64| {
        let low = lag.floor();
        let index = index_at(low);
        let a = correlation.get(index).copied().unwrap_or(0.0);
        let b = correlation.get(index + 1).copied().unwrap_or(0.0);
        a + (b - a) * (lag - low)
    };
    let near = |lag: f64| at(lag.floor()).max(at(lag.ceil()));
    let steps = index_at(((longest - shortest) / LAG_STEP_FRAMES).floor());
    let mut best = None::<(f64, f64)>;
    for lag in (0..=steps)
        .map(|step| shortest + real(step) * LAG_STEP_FRAMES)
        .chain([longest])
    {
        let score = at(lag) * prior(lag);
        if best.is_none_or(|(_, top)| score > top) {
            best = Some((lag, score));
        }
    }
    let (mut lag, score) = best?;
    if score <= 0.0 {
        return None;
    }
    while lag / 2.0 >= shortest && near(lag / 2.0) >= HALF_PERIOD_CORRELATION_SHARE * near(lag) {
        lag /= 2.0;
    }
    Some(lag)
}

/// Ellis 2007, "Beat Tracking by Dynamic Programming", with librosa's start and trim rules.
pub(crate) fn beats(flux: &[f32], period: f64) -> Vec<usize> {
    let len = flux.len();
    if len < 2 || period < 1.0 {
        return Vec::new();
    }
    let mean = flux.iter().map(|&value| f64::from(value)).sum::<f64>() / real(len);
    let variance = flux
        .iter()
        .map(|&value| (f64::from(value) - mean).powi(2))
        .sum::<f64>()
        / real(len - 1);
    if variance <= 0.0 {
        return Vec::new();
    }
    let deviation = variance.sqrt();
    let sigma = period / 32.0;
    let radius = index_at((4.0 * sigma).ceil()).max(1);
    let kernel = (0..=2 * radius)
        .map(|tap| (-0.5 * ((real(tap) - real(radius)) / sigma).powi(2)).exp())
        .collect::<Vec<_>>();
    let local = (0..len)
        .map(|index| {
            kernel
                .iter()
                .enumerate()
                .filter_map(|(tap, weight)| {
                    (index + tap)
                        .checked_sub(radius)
                        .and_then(|at| flux.get(at))
                        .map(|&value| weight * f64::from(value) / deviation)
                })
                .sum::<f64>()
        })
        .collect::<Vec<_>>();
    let nearest = index_at((period / 2.0).round()).max(1);
    let farthest = index_at((2.0 * period).round()).max(nearest);
    let penalty = (nearest..=farthest)
        .map(|distance| -TIGHTNESS * (real(distance) / period).ln().powi(2))
        .collect::<Vec<_>>();
    let start_score = FIRST_BEAT_LOCAL_SCORE_SHARE * local.iter().copied().fold(0.0, f64::max);
    let mut cumulative = vec![0.0; len];
    let mut back = vec![None; len];
    let mut started = false;
    for index in 0..len {
        let mut best = f64::NEG_INFINITY;
        let mut from = None;
        for distance in (nearest..=farthest).rev() {
            let earlier = index.checked_sub(distance);
            let value = penalty[distance - nearest] + earlier.map_or(0.0, |at| cumulative[at]);
            if value > best {
                best = value;
                from = earlier;
            }
        }
        cumulative[index] = local[index] + best;
        if started || local[index] >= start_score {
            started = true;
            back[index] = from;
        }
    }
    let maxima = (1..len)
        .filter(|&index| {
            cumulative[index] > cumulative[index - 1]
                && (index + 1 == len || cumulative[index] >= cumulative[index + 1])
        })
        .collect::<Vec<_>>();
    let mut peaks = maxima
        .iter()
        .map(|&index| cumulative[index])
        .collect::<Vec<_>>();
    peaks.sort_by(f64::total_cmp);
    let Some(&median) = peaks.get(peaks.len() / 2) else {
        return Vec::new();
    };
    let Some(&last) = maxima
        .iter()
        .rev()
        .find(|&&index| 2.0 * cumulative[index] > median)
    else {
        return Vec::new();
    };
    let mut chain = vec![last];
    while let Some(earlier) = chain.last().and_then(|&index| back[index]) {
        chain.push(earlier);
    }
    chain.reverse();
    trimmed(&chain, &local)
}

fn trimmed(chain: &[usize], local: &[f64]) -> Vec<usize> {
    let strength = chain.iter().map(|&index| local[index]).collect::<Vec<_>>();
    let smooth = (0..strength.len())
        .map(|at| {
            let side = |other: Option<usize>| {
                other
                    .and_then(|other| strength.get(other))
                    .copied()
                    .unwrap_or(0.0)
            };
            0.5 * side(at.checked_sub(1)) + strength[at] + 0.5 * side(Some(at + 1))
        })
        .collect::<Vec<_>>();
    let threshold =
        0.5 * (smooth.iter().map(|value| value * value).sum::<f64>() / real(smooth.len())).sqrt();
    let first = smooth.iter().position(|&value| value > threshold);
    let last = smooth.iter().rposition(|&value| value > threshold);
    match (first, last) {
        (Some(first), Some(last)) => chain[first..=last].to_vec(),
        _ => Vec::new(),
    }
}
