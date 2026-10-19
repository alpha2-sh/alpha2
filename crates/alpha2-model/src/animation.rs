use crate::{CubicBezier, Easing};
use serde::{Deserialize, Serialize};

/// A two-dimensional value in normalized output coordinates.
#[derive(Copy, Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct Vec2 {
    /// Horizontal component.
    pub x: f32,
    /// Vertical component.
    pub y: f32,
}

impl Vec2 {
    /// Creates a two-dimensional value.
    #[must_use]
    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// A value that is either constant or controlled by sorted keyframes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(feature = "schema", schemars(rename = "Animated_{T}"))]
pub enum Animated<T> {
    /// A value without animation.
    Constant {
        /// Value used at every timestamp.
        value: T,
    },
    /// Values evaluated from their keyframe sequence.
    Keyframes {
        /// Strictly ascending keyframes. The evaluator clamps outside the sequence.
        keyframes: Vec<Keyframe<T>>,
    },
}

/// One timestamped control value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", schemars(rename = "Keyframe_{T}"))]
pub struct Keyframe<T> {
    /// Timestamp of this value: timeline time, except in a composition
    /// node's settings, which are keyed in composition time.
    pub time: alpha2_time::MediaTime,
    /// Value at `time`.
    pub value: T,
    /// Interpolation from this keyframe to its successor.
    pub interpolation: Interpolation,
}

/// Interpolation from one keyframe to the next.
///
/// Values are interpolated numerically: a rotation from 0 to 720 degrees
/// turns twice, and one from 0 to 360 turns once. A host that wants the
/// shortest path normalizes its keyframe values.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "type", rename_all = "snake_case", from = "InterpolationWire")]
pub enum Interpolation {
    /// Keep this keyframe's value until the next keyframe.
    Hold,
    /// Move to the next value at a constant rate.
    Linear,
    /// Move to the next value along a CSS `cubic-bezier()` timing curve.
    CubicBezier(CubicBezier),
}

/// The wire form of [`Interpolation`]; see `EasingWire` for why the unit
/// variants are empty structs here.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "Interpolation"))]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum InterpolationWire {
    /// Keep this keyframe's value until the next keyframe.
    Hold {},
    /// Move to the next value at a constant rate.
    Linear {},
    /// Move to the next value along a CSS `cubic-bezier()` timing curve.
    CubicBezier(CubicBezier),
}

impl From<InterpolationWire> for Interpolation {
    fn from(wire: InterpolationWire) -> Self {
        match wire {
            InterpolationWire::Hold {} => Self::Hold,
            InterpolationWire::Linear {} => Self::Linear,
            InterpolationWire::CubicBezier(curve) => Self::CubicBezier(curve),
        }
    }
}

impl Interpolation {
    /// The fraction of the way to the next value at linear segment
    /// `progress` (clamped to `0..=1`), or `None` while holding.
    ///
    /// A cubic Bezier curve may overshoot, so the fraction can leave `0..=1`.
    #[must_use]
    pub fn amount(self, progress: f64) -> Option<f64> {
        match self {
            Self::Hold => None,
            Self::Linear => Some(Easing::Linear.apply(progress)),
            Self::CubicBezier(curve) => Some(curve.apply(progress)),
        }
    }

    /// Whether the interpolation's parameters are usable.
    #[must_use]
    pub fn is_valid(self) -> bool {
        match self {
            Self::Hold | Self::Linear => true,
            Self::CubicBezier(curve) => curve.is_valid(),
        }
    }
}

/// Resolution-independent visual presentation controls for a clip.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct VisualProperties {
    /// Animated normalized canvas translation.
    pub position: Animated<Vec2>,
    /// Unanimated normalized point within the source rectangle.
    pub anchor: Vec2,
    /// Animated per-axis scale.
    pub scale: Animated<Vec2>,
    /// Animated clockwise rotation in degrees.
    pub rotation_degrees: Animated<f32>,
    /// Animated opacity constrained to `0..=1`.
    pub opacity: Animated<f32>,
}

impl Default for VisualProperties {
    fn default() -> Self {
        Self {
            position: Animated::Constant {
                value: Vec2::new(0.5, 0.5),
            },
            anchor: Vec2::new(0.5, 0.5),
            scale: Animated::Constant {
                value: Vec2::new(1.0, 1.0),
            },
            rotation_degrees: Animated::Constant { value: 0.0 },
            opacity: Animated::Constant { value: 1.0 },
        }
    }
}

impl VisualProperties {
    pub(crate) fn validate(&self) -> bool {
        self.anchor.is_finite()
            && valid_vec2_animation(&self.position)
            && valid_vec2_animation(&self.scale)
            && valid_scalar_animation(&self.rotation_degrees, false)
            && valid_scalar_animation(&self.opacity, true)
    }
}

fn valid_vec2_animation(animation: &Animated<Vec2>) -> bool {
    match animation {
        Animated::Constant { value } => value.is_finite(),
        Animated::Keyframes { keyframes } => {
            valid_keyframes(keyframes)
                && keyframes.iter().all(|keyframe| keyframe.value.is_finite())
        }
    }
}

fn valid_scalar_animation(animation: &Animated<f32>, unit_interval: bool) -> bool {
    let valid_value =
        |value: f32| value.is_finite() && (!unit_interval || (0.0..=1.0).contains(&value));
    match animation {
        Animated::Constant { value } => valid_value(*value),
        Animated::Keyframes { keyframes } => {
            valid_keyframes(keyframes)
                && keyframes.iter().all(|keyframe| valid_value(keyframe.value))
        }
    }
}

/// Valid keyframes (or a constant) whose every finite value passes `check`.
pub(crate) fn valid_scalar_values(animation: &Animated<f32>, check: impl Fn(f32) -> bool) -> bool {
    let valid = |value: f32| value.is_finite() && check(value);
    match animation {
        Animated::Constant { value } => valid(*value),
        Animated::Keyframes { keyframes } => {
            valid_keyframes(keyframes) && keyframes.iter().all(|keyframe| valid(keyframe.value))
        }
    }
}

/// [`valid_scalar_values`] for two-dimensional values.
pub(crate) fn valid_vec2_values(animation: &Animated<Vec2>, check: impl Fn(Vec2) -> bool) -> bool {
    let valid = |value: Vec2| value.is_finite() && check(value);
    match animation {
        Animated::Constant { value } => valid(*value),
        Animated::Keyframes { keyframes } => {
            valid_keyframes(keyframes) && keyframes.iter().all(|keyframe| valid(keyframe.value))
        }
    }
}

/// A non-empty, strictly ascending sequence whose interpolations are usable.
pub(crate) fn valid_keyframes<T>(keyframes: &[Keyframe<T>]) -> bool {
    !keyframes.is_empty()
        && keyframes.windows(2).all(|pair| pair[0].time < pair[1].time)
        && keyframes
            .iter()
            .all(|keyframe| keyframe.interpolation.is_valid())
}
