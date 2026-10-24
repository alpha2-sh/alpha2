//! Value checks shared by the shapes and layout validators: each names where
//! a value lies and what is wrong with it.

use crate::animation::{valid_keyframes, valid_point_animation};
use crate::{Animated, AnimatedPoint, Keyframe, Paint, Vec2};

/// What is wrong with one value.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum ValueProblem {
    /// Keyframes that are empty, out of order, or have an unusable curve.
    InvalidKeyframes,
    /// A value that is not finite or outside its range.
    OutOfRange,
    /// Paint keyframes that differ in kind, stop count, or spread.
    PaintKeyframesDiffer,
}

/// Where a value lies, and what is wrong with it.
#[derive(Clone, Debug)]
pub(crate) struct ValueIssue {
    pub(crate) at: String,
    pub(crate) problem: ValueProblem,
}

impl ValueIssue {
    pub(crate) fn new(at: impl Into<String>, problem: ValueProblem) -> Self {
        Self {
            at: at.into(),
            problem,
        }
    }
}

pub(crate) type Checked = Result<(), ValueIssue>;

pub(crate) fn unit(value: f32) -> bool {
    (0.0..=1.0).contains(&value)
}

pub(crate) fn non_negative(value: f32) -> bool {
    value >= 0.0
}

pub(crate) fn any(_: f32) -> bool {
    true
}

/// Valid keyframes (or a constant) whose every finite value passes `check`.
pub(crate) fn animated<T>(
    animation: &Animated<T>,
    at: &str,
    check: impl Fn(&T) -> bool,
) -> Checked {
    let values_valid = match animation {
        Animated::Constant { value } => check(value),
        Animated::Keyframes { keyframes } => {
            if !valid_keyframes(keyframes) {
                return Err(ValueIssue::new(at, ValueProblem::InvalidKeyframes));
            }
            keyframes.iter().all(|keyframe| check(&keyframe.value))
        }
    };
    values_valid
        .then_some(())
        .ok_or_else(|| ValueIssue::new(at, ValueProblem::OutOfRange))
}

pub(crate) fn scalar(animation: &Animated<f32>, at: &str, check: fn(f32) -> bool) -> Checked {
    animated(animation, at, |value| value.is_finite() && check(*value))
}

pub(crate) fn vec2(animation: &Animated<Vec2>, at: &str, check: fn(Vec2) -> bool) -> Checked {
    animated(animation, at, |value| {
        value.x.is_finite() && value.y.is_finite() && check(*value)
    })
}

pub(crate) fn point(animation: &AnimatedPoint, at: &str) -> Checked {
    if let AnimatedPoint::Keyframes { keyframes } = animation
        && (keyframes.is_empty()
            || keyframes
                .windows(2)
                .any(|pair| pair[0].time >= pair[1].time)
            || !keyframes
                .iter()
                .all(|keyframe| keyframe.interpolation.is_valid()))
    {
        return Err(ValueIssue::new(at, ValueProblem::InvalidKeyframes));
    }
    valid_point_animation(animation)
        .then_some(())
        .ok_or_else(|| ValueIssue::new(at, ValueProblem::OutOfRange))
}

pub(crate) fn validate_paint(paint: &Animated<Paint>, at: &str) -> Checked {
    animated(paint, at, Paint::is_valid)?;
    if let Animated::Keyframes { keyframes } = paint {
        let first = paint_structure(&keyframes[0]);
        if keyframes[1..]
            .iter()
            .any(|keyframe| paint_structure(keyframe) != first)
        {
            return Err(ValueIssue::new(at, ValueProblem::PaintKeyframesDiffer));
        }
    }
    Ok(())
}

/// What two paint keyframes must share to interpolate: kind, stop count, and
/// spread.
fn paint_structure(keyframe: &Keyframe<Paint>) -> (u8, usize, Option<crate::Spread>) {
    match &keyframe.value {
        Paint::Solid { .. } => (0, 1, None),
        Paint::LinearGradient { stops, spread, .. } => (1, stops.len(), Some(*spread)),
        Paint::RadialGradient { stops, spread, .. } => (2, stops.len(), Some(*spread)),
        Paint::ConicGradient { stops, .. } => (3, stops.len(), None),
    }
}
