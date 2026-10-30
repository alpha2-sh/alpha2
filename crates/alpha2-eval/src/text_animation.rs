//! Text animators sampled at a time: the plain numbers each animator applies.

use alpha2_geometry::StableHash;
use alpha2_model::{RangeShape, RangeUnits, RunState, TextAnimator, TextSelector, TextUnit};
use alpha2_time::MediaTime;

use crate::{sample_rgba, sample_scalar, sample_vec2};

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
                        start: sample_scalar(start, time),
                        end: sample_scalar(end, time),
                        offset: sample_scalar(offset, time),
                        shape: *shape,
                        amount: sample_scalar(amount, time).clamp(0.0, 1.0),
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
                offset: properties
                    .offset
                    .as_ref()
                    .map(|value| pair(sample_vec2(value, time))),
                scale: properties
                    .scale
                    .as_ref()
                    .map(|value| pair(sample_vec2(value, time)).map(|axis| axis.max(0.0))),
                rotation_degrees: properties
                    .rotation_degrees
                    .as_ref()
                    .map(|value| sample_scalar(value, time)),
                opacity: properties
                    .opacity
                    .as_ref()
                    .map(|value| sample_scalar(value, time).clamp(0.0, 1.0)),
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
                    .map(|value| sample_scalar(value, time)),
            }
        })
        .collect()
}

/// Feeds every value of `animators` to `hash`, for a key that changes as
/// what they draw does.
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
    }
}
