//! Text animators applied to a laid-out text: each cluster's selection by
//! each animator, and the state its properties give it.
//!
//! Selection follows After Effects' text animators. A Range selector counts
//! units (characters, characters that are not spaces, words, or lines) and
//! shapes its range at the ends; a Timing selector picks the characters of
//! runs by where the run's time lies against the current time; a Class
//! selector picks the characters of runs that carry a class. Only `+ - * /`
//! and `sqrt` enter the selection, so native and wasm agree bit for bit.

use alpha2_eval::{SampledAnimator, SampledSelector};
use alpha2_model::{RangeShape, RangeUnits, RunState, TextRun, TextUnit};
use alpha2_text::{ClusterState, ColorMix, TextLayout};
use alpha2_time::MediaTime;

/// The selection, in `[0, 1]`, `selector` gives each cluster of `layout`,
/// at `time` for a Timing selector. `runs` are the text's runs, none for a
/// plain string.
#[must_use]
pub fn selection(
    selector: &SampledSelector,
    layout: &TextLayout,
    runs: &[TextRun],
    time: MediaTime,
) -> Vec<f32> {
    match selector {
        SampledSelector::Range {
            based_on,
            units,
            start,
            end,
            offset,
            shape,
            amount,
        } => {
            let (unit_of, count) = units_of(layout, *based_on);
            #[allow(clippy::cast_precision_loss)]
            let count_f = count as f32;
            let (mut s, mut e) = match units {
                RangeUnits::Percent => (
                    (start + offset) / 100.0 * count_f,
                    (end + offset) / 100.0 * count_f,
                ),
                RangeUnits::Index => (start + offset, end + offset),
            };
            if s > e {
                std::mem::swap(&mut s, &mut e);
            }
            unit_of
                .iter()
                .map(|unit| unit.map_or(0.0, |unit| range_value(*shape, unit, s, e) * amount))
                .collect()
        }
        SampledSelector::Timing { state, ramp, sweep } => {
            timing(layout, runs, time, *state, *ramp, *sweep)
        }
        SampledSelector::Class { class } => {
            let run_of = run_of_clusters(layout, runs);
            run_of
                .iter()
                .map(|run| {
                    let selected =
                        run.is_some_and(|run| runs[run].class.iter().any(|name| name == class));
                    if selected { 1.0 } else { 0.0 }
                })
                .collect()
        }
    }
}

/// Each cluster's unit index under `based_on`, `None` for a cluster in no
/// unit (a space under `words`), and how many units there are.
fn units_of(layout: &TextLayout, based_on: TextUnit) -> (Vec<Option<u32>>, u32) {
    let clusters = &layout.clusters;
    match based_on {
        TextUnit::Characters => (
            (0..u32::try_from(clusters.len()).unwrap_or(u32::MAX))
                .map(Some)
                .collect(),
            u32::try_from(clusters.len()).unwrap_or(u32::MAX),
        ),
        TextUnit::CharactersExcludingSpaces => {
            let mut next = 0;
            let units = clusters
                .iter()
                .map(|cluster| {
                    (!cluster.is_space).then(|| {
                        next += 1;
                        next - 1
                    })
                })
                .collect();
            (units, next)
        }
        TextUnit::Words => {
            let count = clusters
                .iter()
                .filter_map(|cluster| cluster.word)
                .max()
                .map_or(0, |last| last + 1);
            (clusters.iter().map(|cluster| cluster.word).collect(), count)
        }
        TextUnit::Lines => (
            clusters.iter().map(|cluster| Some(cluster.line)).collect(),
            u32::try_from(layout.lines.len()).unwrap_or(u32::MAX),
        ),
    }
}

/// The value `shape` gives unit `unit` (covering `[unit, unit + 1)`) of a
/// range from `s` to `e` in unit space, `s <= e`.
#[allow(clippy::cast_precision_loss, clippy::many_single_char_names)]
fn range_value(shape: RangeShape, unit: u32, s: f32, e: f32) -> f32 {
    #[allow(clippy::float_cmp)]
    if e == s {
        return 0.0;
    }
    let i = unit as f32;
    let ramp = || (i + 0.5 - s) / (e - s);
    let triangle = |u: f32| {
        if (0.0..=1.0).contains(&u) {
            1.0 - (2.0 * u - 1.0).abs()
        } else {
            0.0
        }
    };
    match shape {
        RangeShape::Square => (e.min(i + 1.0) - s.max(i)).clamp(0.0, 1.0),
        RangeShape::RampUp => ramp().clamp(0.0, 1.0),
        RangeShape::RampDown => 1.0 - ramp().clamp(0.0, 1.0),
        RangeShape::Triangle => triangle(ramp()),
        RangeShape::Round => {
            let u = ramp();
            if (0.0..=1.0).contains(&u) {
                let x = 2.0 * u - 1.0;
                (1.0 - x * x).max(0.0).sqrt()
            } else {
                0.0
            }
        }
        RangeShape::Smooth => {
            let t = triangle(ramp());
            t * t * (3.0 - 2.0 * t)
        }
    }
}

/// The run each cluster belongs to, by the run that holds its first byte;
/// `None` without runs.
fn run_of_clusters(layout: &TextLayout, runs: &[TextRun]) -> Vec<Option<usize>> {
    let mut ends = Vec::with_capacity(runs.len());
    let mut end = 0;
    for run in runs {
        end += run.text.len();
        ends.push(end);
    }
    layout
        .clusters
        .iter()
        .map(|cluster| {
            let run = ends.partition_point(|&end| end <= cluster.bytes.start);
            (run < runs.len()).then_some(run)
        })
        .collect()
}

/// A Timing selector's selection of each cluster at `time`.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
fn timing(
    layout: &TextLayout,
    runs: &[TextRun],
    time: MediaTime,
    state: RunState,
    ramp: MediaTime,
    sweep: bool,
) -> Vec<f32> {
    let run_of = run_of_clusters(layout, runs);
    let ramp = ramp.ticks();
    // How far past `at` the time is, faded over the ramp: a step without one.
    let reached = |at: i64| -> f32 {
        let past = time.ticks().saturating_sub(at);
        if ramp <= 0 {
            if past >= 0 { 1.0 } else { 0.0 }
        } else {
            (past as f64 / ramp as f64).clamp(0.0, 1.0) as f32
        }
    };
    // Each run's clusters in order, for a sweep's share of the run.
    let mut place = vec![(0_usize, 0_usize); run_of.len()];
    let mut counts = vec![0_usize; runs.len()];
    for (cluster, run) in run_of.iter().enumerate() {
        if let Some(run) = run {
            place[cluster].0 = counts[*run];
            counts[*run] += 1;
        }
    }
    for (cluster, run) in run_of.iter().enumerate() {
        if let Some(run) = run {
            place[cluster].1 = counts[*run];
        }
    }
    run_of
        .iter()
        .zip(place)
        .map(|(run, (index, count))| {
            let Some(span) = run.and_then(|run| runs[run].time) else {
                return 0.0;
            };
            let (start, end) = (span.start.ticks(), span.end.ticks());
            let (a, b) = if sweep && count > 0 {
                let length = i128::from(end - start);
                let share = |at: usize| {
                    i64::try_from(i128::from(start) + length * at as i128 / count as i128)
                        .unwrap_or(end)
                };
                (share(index), share(index + 1))
            } else {
                (start, end)
            };
            let (begun, ended) = (reached(a), reached(b));
            match state {
                RunState::Upcoming => 1.0 - begun,
                RunState::Active => begun * (1.0 - ended),
                RunState::Spoken => ended,
            }
        })
        .collect()
}

/// Each cluster's state under `animators` at `time`, in pixels of a
/// `font_size`-pixel font, and a hash of the states: 0 when every state is
/// identity, so the text draws as it would without animators. `base_fill`
/// and `base_stroke` are what fill and stroke mixes start from.
#[must_use]
pub fn cluster_states(
    layout: &TextLayout,
    runs: &[TextRun],
    animators: &[SampledAnimator],
    time: MediaTime,
    font_size: f32,
) -> (Vec<ClusterState>, u64) {
    let mut states = vec![ClusterState::IDENTITY; layout.clusters.len()];
    // Rotation is summed in degrees and turned into radians once.
    let mut degrees = vec![0.0_f32; states.len()];
    for animator in animators {
        let weights = selection(&animator.selector, layout, runs, time);
        for ((state, degrees), weight) in states.iter_mut().zip(&mut degrees).zip(weights) {
            if weight <= 0.0 {
                continue;
            }
            if let Some([x, y]) = animator.offset {
                state.offset[0] += weight * x * font_size;
                state.offset[1] += weight * y * font_size;
            }
            if let Some(scale) = animator.scale {
                for (axis, target) in state.scale.iter_mut().zip(scale) {
                    *axis *= 1.0 + weight * (target - 1.0);
                }
            }
            if let Some(rotation) = animator.rotation_degrees {
                *degrees += weight * rotation;
            }
            if let Some(opacity) = animator.opacity {
                state.opacity *= 1.0 + weight * (opacity - 1.0);
            }
            if let Some(target) = animator.fill {
                state.fill = Some(mix_toward(state.fill, target, weight));
            }
            if let Some(target) = animator.stroke {
                state.stroke = Some(mix_toward(state.stroke, target, weight));
            }
            if let Some(tracking) = animator.tracking {
                state.tracking += weight * tracking * font_size;
            }
        }
    }
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    let mut feed = |value: f32| {
        for byte in value.to_bits().to_le_bytes() {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    let mut identity = true;
    for (state, degrees) in states.iter_mut().zip(degrees) {
        state.rotation = degrees.to_radians();
        state.opacity = state.opacity.clamp(0.0, 1.0);
        identity &= state.is_identity();
        let mixes = [state.fill, state.stroke].map(|mix| {
            mix.map_or([1.0, 0.0, 0.0, 0.0, 0.0], |mix| {
                [mix.keep, mix.add[0], mix.add[1], mix.add[2], mix.add[3]]
            })
        });
        for value in state
            .offset
            .into_iter()
            .chain(state.scale)
            .chain([state.rotation, state.opacity, state.tracking])
            .chain(mixes.into_iter().flatten())
        {
            feed(value);
        }
    }
    (states, if identity { 0 } else { hash.max(1) })
}

/// `mix` (or the color unchanged) mixed toward the straight sRGB `target`
/// by `weight`, in premultiplied linear light.
fn mix_toward(mix: Option<ColorMix>, target: [f32; 4], weight: f32) -> ColorMix {
    let mix = mix.unwrap_or(ColorMix {
        keep: 1.0,
        add: [0.0; 4],
    });
    let target = premultiplied_linear(target);
    ColorMix {
        keep: mix.keep * (1.0 - weight),
        add: std::array::from_fn(|channel| {
            mix.add[channel] * (1.0 - weight) + target[channel] * weight
        }),
    }
}

/// Straight sRGB to premultiplied linear light, as paint stops convert.
fn premultiplied_linear(color: [f32; 4]) -> [f32; 4] {
    let alpha = color[3].clamp(0.0, 1.0);
    let linear = |value: f32| {
        let value = value.clamp(0.0, 1.0);
        if value >= 1.0 {
            1.0
        } else if value <= 0.040_45 {
            value / 12.92
        } else {
            libm::powf((value + 0.055) / 1.055, 2.4)
        }
    };
    [
        linear(color[0]) * alpha,
        linear(color[1]) * alpha,
        linear(color[2]) * alpha,
        alpha,
    ]
}

#[cfg(test)]
#[path = "text_animation_tests.rs"]
mod tests;
