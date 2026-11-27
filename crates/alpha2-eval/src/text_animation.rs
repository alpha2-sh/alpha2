//! Text animators sampled at a time: the plain numbers each animator applies.

use alpha2_geometry::StableHash;
use alpha2_model::{
    Animated, Interpolation, Keyframe, RangeShape, RangeUnits, RunState, TextAnimator, TextRun,
    TextSelector, TextUnit,
};
use alpha2_time::MediaTime;

use crate::{SampleContext, sample_rgba, sample_scalar, sample_vec2};

/// An animator's selector and properties at one time.
#[derive(Clone, Debug, PartialEq)]
pub struct SampledAnimator {
    /// What it selects.
    pub selector: SampledSelector,
    /// Offset in font sizes, +y down.
    pub offset: Option<[f32; 2]>,
    /// Scale about each character's centre.
    pub scale: Option<[f32; 2]>,
    /// Clockwise degrees about each character's centre.
    pub rotation_degrees: Option<f32>,
    /// Target opacity.
    pub opacity: Option<f32>,
    /// Target fill color, straight sRGB.
    pub fill: Option<[f32; 4]>,
    /// Target stroke color, straight sRGB.
    pub stroke: Option<[f32; 4]>,
    /// Extra space after each character, font sizes.
    pub tracking: Option<f32>,
    /// Whether the advance follows the horizontal scale.
    pub advance_follows_scale: bool,
}

/// A selector at one time.
#[derive(Clone, Debug, PartialEq)]
pub enum SampledSelector {
    /// A Range selector's values.
    Range {
        /// What it counts.
        based_on: TextUnit,
        /// Percents or indices.
        units: RangeUnits,
        /// Start, with the offset not added.
        start: f32,
        /// End, with the offset not added.
        end: f32,
        /// Added to start and end.
        offset: f32,
        /// How selection rises and falls.
        shape: RangeShape,
        /// Selection scale.
        amount: f32,
    },
    /// A Timing selector (its values are not animated).
    Timing {
        /// Which runs.
        state: RunState,
        /// Fade length.
        ramp: MediaTime,
        /// Sweep each run's characters in order.
        sweep: bool,
    },
    /// A Class selector.
    Class {
        /// The class name.
        class: String,
    },
}

/// `animators` sampled at `time`, in order.
#[must_use]
pub fn sample_text_animators(animators: &[TextAnimator], time: MediaTime) -> Vec<SampledAnimator> {
    animators
        .iter()
        .map(|animator| {
            let properties = &animator.properties;
            let pair = |value: alpha2_model::Vec2| [value.x, value.y];
            let color =
                |value: alpha2_model::Rgba| [value.red, value.green, value.blue, value.alpha];
            SampledAnimator {
                selector: match &animator.selector {
                    TextSelector::Range {
                        based_on,
                        units,
                        start,
                        end,
                        offset,
                        shape,
                        amount,
                    } => SampledSelector::Range {
                        based_on: *based_on,
                        units: *units,
                        start: sample_scalar(start, time, &SampleContext::NONE),
                        end: sample_scalar(end, time, &SampleContext::NONE),
                        offset: sample_scalar(offset, time, &SampleContext::NONE),
                        shape: *shape,
                        amount: sample_scalar(amount, time, &SampleContext::NONE).clamp(0.0, 1.0),
                    },
                    TextSelector::Timing { state, ramp, sweep } => SampledSelector::Timing {
                        state: *state,
                        ramp: *ramp,
                        sweep: *sweep,
                    },
                    TextSelector::Class { class } => SampledSelector::Class {
                        class: class.clone(),
                    },
                },
                offset: properties.offset.as_ref().map(|value| {
                    let bound = alpha2_model::MAX_ANIMATOR_OFFSET;
                    pair(sample_vec2(value, time, &SampleContext::NONE))
                        .map(|axis| axis.clamp(-bound, bound))
                }),
                scale: properties.scale.as_ref().map(|value| {
                    pair(sample_vec2(value, time, &SampleContext::NONE)).map(|axis| axis.max(0.0))
                }),
                rotation_degrees: properties
                    .rotation_degrees
                    .as_ref()
                    .map(|value| sample_scalar(value, time, &SampleContext::NONE)),
                opacity: properties
                    .opacity
                    .as_ref()
                    .map(|value| sample_scalar(value, time, &SampleContext::NONE).clamp(0.0, 1.0)),
                fill: properties
                    .fill
                    .as_ref()
                    .map(|value| color(sample_rgba(value, time))),
                stroke: properties
                    .stroke
                    .as_ref()
                    .map(|value| color(sample_rgba(value, time))),
                tracking: properties
                    .tracking
                    .as_ref()
                    .map(|value| sample_scalar(value, time, &SampleContext::NONE)),
                advance_follows_scale: properties.advance_follows_scale,
            }
        })
        .collect()
}

/// Feeds every value of `animators` to `hash`.
pub fn hash_sampled_animators(hash: &mut StableHash, animators: &[SampledAnimator]) {
    hash.u64(animators.len() as u64);
    for animator in animators {
        match &animator.selector {
            SampledSelector::Range {
                based_on,
                units,
                start,
                end,
                offset,
                shape,
                amount,
            } => {
                hash.u32(0);
                hash.u32(*based_on as u32);
                hash.u32(*units as u32);
                hash.u32(*shape as u32);
                for value in [start, end, offset, amount] {
                    hash.f32(*value);
                }
            }
            SampledSelector::Timing { state, ramp, sweep } => {
                hash.u32(1);
                hash.u32(*state as u32);
                hash.u64(ramp.ticks().cast_unsigned());
                hash.u32(u32::from(*sweep));
            }
            SampledSelector::Class { class } => {
                hash.u32(2);
                hash.bytes(class.as_bytes());
            }
        }
        let mut option = |values: Option<&[f32]>| match values {
            None => hash.u32(0),
            Some(values) => {
                hash.u32(1);
                for value in values {
                    hash.f32(*value);
                }
            }
        };
        option(animator.offset.as_ref().map(<[f32; 2]>::as_slice));
        option(animator.scale.as_ref().map(<[f32; 2]>::as_slice));
        option(animator.rotation_degrees.as_ref().map(std::slice::from_ref));
        option(animator.opacity.as_ref().map(std::slice::from_ref));
        option(animator.fill.as_ref().map(<[f32; 4]>::as_slice));
        option(animator.stroke.as_ref().map(<[f32; 4]>::as_slice));
        option(animator.tracking.as_ref().map(std::slice::from_ref));
        if animator.advance_follows_scale {
            hash.u32(0xadf5);
        }
    }
}

/// The largest factor any cluster's advance can be widened by under `animators` over a text of
/// `runs`: per animator whose advance follows its scale, `1 + w (s - 1)` with `s` its largest
/// horizontal scale and `w` its largest selection, at least 1; their product, at most the
/// largest animator scale, which also bounds a cluster's. 1 when none pushes.
///
/// Keyframes count every value and each eased segment's extremes, so an overshooting curve
/// reserves its overshoot. A value bound to data, which text animators refuse, takes its range's
/// bound.
#[must_use]
pub fn advance_headroom(animators: &[TextAnimator], runs: &[TextRun]) -> f32 {
    let mut headroom = 1.0_f64;
    for animator in animators {
        let properties = &animator.properties;
        let Some(scale) = properties
            .scale
            .as_ref()
            .filter(|_| properties.advance_follows_scale)
        else {
            continue;
        };
        let largest_scale = largest(scale, |value| value.x, *ANIMATOR_SCALE.end()).max(0.0);
        let selection = match &animator.selector {
            TextSelector::Range { amount, .. } => {
                largest(amount, |value| *value, 1.0).clamp(0.0, 1.0)
            }
            TextSelector::Timing { .. } => {
                if runs.iter().any(|run| run.time.is_some()) {
                    1.0
                } else {
                    0.0
                }
            }
            TextSelector::Class { class } => {
                if runs.iter().any(|run| run.class.contains(class)) {
                    1.0
                } else {
                    0.0
                }
            }
        };
        headroom *= (1.0 + selection * (largest_scale - 1.0)).max(1.0);
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "at most the cluster scale limit"
    )]
    let headroom = headroom.min(f64::from(*ANIMATOR_SCALE.end())) as f32;
    headroom
}

const ANIMATOR_SCALE: std::ops::RangeInclusive<f32> = alpha2_model::ANIMATOR_SCALE_RANGE;

/// The largest `component` an animated value takes, `bound` for one bound to data.
fn largest<T>(animated: &Animated<T>, component: impl Fn(&T) -> f32, bound: f32) -> f64 {
    match animated {
        Animated::Constant { value } => f64::from(component(value)),
        Animated::Keyframes { keyframes } => keyframes
            .iter()
            .zip(keyframes.iter().skip(1).map(Some).chain([None]))
            .map(|(left, right)| segment_largest(left, right, &component))
            .fold(f64::NEG_INFINITY, f64::max),
        Animated::Data(_) => f64::from(bound),
    }
}

/// The largest `component` from keyframe `left` to `right`, the left value alone for the last.
fn segment_largest<T>(
    left: &Keyframe<T>,
    right: Option<&Keyframe<T>>,
    component: &impl Fn(&T) -> f32,
) -> f64 {
    let from = f64::from(component(&left.value));
    let Some(right) = right else {
        return from;
    };
    let to = f64::from(component(&right.value));
    let [low, high] = match left.interpolation {
        Interpolation::Hold => return from,
        Interpolation::Linear => [0.0, 1.0],
        Interpolation::CubicBezier(curve) => {
            bezier_output_range(f64::from(curve.y1), f64::from(curve.y2))
        }
    };
    (from + (to - from) * low).max(from + (to - from) * high)
}

/// The least and largest output of a `cubic-bezier` with these `y1`, `y2` over its whole
/// parameter range: its ends and where its derivative, a quadratic, is zero.
fn bezier_output_range(y1: f64, y2: f64) -> [f64; 2] {
    let output = |t: f64| {
        let rest = 1.0 - t;
        3.0 * rest * rest * t * y1 + 3.0 * rest * t * t * y2 + t * t * t
    };
    let (d0, d1, d2) = (y1, y2 - y1, 1.0 - y2);
    let (a, b, c) = (d0 - 2.0 * d1 + d2, 2.0 * (d1 - d0), d0);
    let mut roots = [None, None];
    if a.abs() <= f64::EPSILON {
        if b.abs() > f64::EPSILON {
            roots[0] = Some(-c / b);
        }
    } else {
        let discriminant = b * b - 4.0 * a * c;
        if discriminant >= 0.0 {
            let root = discriminant.sqrt();
            roots = [Some((-b + root) / (2.0 * a)), Some((-b - root) / (2.0 * a))];
        }
    }
    roots
        .into_iter()
        .flatten()
        .filter(|t| *t > 0.0 && *t < 1.0)
        .map(output)
        .fold([0.0, 1.0], |[low, high], value| {
            [low.min(value), high.max(value)]
        })
}
