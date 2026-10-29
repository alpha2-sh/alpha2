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

    /// Whether both components are zero (either sign).
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.x == 0.0 && self.y == 0.0
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

impl<T> Animated<T> {
    /// The same animation with each value mapped by `map`, its keyframes'
    /// times and interpolations kept.
    #[must_use]
    pub fn map<U>(&self, map: impl Fn(&T) -> U) -> Animated<U> {
        match self {
            Self::Constant { value } => Animated::Constant { value: map(value) },
            Self::Keyframes { keyframes } => Animated::Keyframes {
                keyframes: keyframes
                    .iter()
                    .map(|keyframe| Keyframe {
                        time: keyframe.time,
                        value: map(&keyframe.value),
                        interpolation: keyframe.interpolation,
                    })
                    .collect(),
            },
        }
    }
}

/// A position that can move along curves between its keyframes.
///
/// Its JSON is that of an `Animated<Vec2>`, plus optional spatial tangents on
/// each keyframe: with them, the value travels along the cubic Bezier from
/// one keyframe's value through its `out_tangent` and the next keyframe's
/// `in_tangent`, at the speed its timing curve sets along the curve's
/// length. Without them it moves in a straight line, as an `Animated<Vec2>`
/// does.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AnimatedPoint {
    /// A value without animation.
    Constant {
        /// Value used at every timestamp.
        value: Vec2,
    },
    /// Values evaluated from their keyframe sequence.
    Keyframes {
        /// Strictly ascending keyframes. The evaluator clamps outside the sequence.
        keyframes: Vec<PointKeyframe>,
    },
}

impl Default for AnimatedPoint {
    fn default() -> Self {
        Self::Constant {
            value: Vec2::default(),
        }
    }
}

impl From<Animated<Vec2>> for AnimatedPoint {
    fn from(animation: Animated<Vec2>) -> Self {
        match animation {
            Animated::Constant { value } => Self::Constant { value },
            Animated::Keyframes { keyframes } => Self::Keyframes {
                keyframes: keyframes
                    .into_iter()
                    .map(|keyframe| PointKeyframe {
                        time: keyframe.time,
                        value: keyframe.value,
                        interpolation: keyframe.interpolation,
                        out_tangent: Vec2::default(),
                        in_tangent: Vec2::default(),
                    })
                    .collect(),
            },
        }
    }
}

impl AnimatedPoint {
    /// Whether the value changes over time.
    #[must_use]
    pub const fn is_animated(&self) -> bool {
        matches!(self, Self::Keyframes { .. })
    }
}

/// One keyframe of an [`AnimatedPoint`].
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct PointKeyframe {
    /// Timestamp of this value, as a [`Keyframe`]'s.
    pub time: alpha2_time::MediaTime,
    /// Value at `time`.
    pub value: Vec2,
    /// Interpolation from this keyframe to its successor: how fast the value
    /// travels along the way there.
    pub interpolation: Interpolation,
    /// The start of the way to the next keyframe's value, relative to this
    /// value: the curve's first control point.
    #[serde(default, skip_serializing_if = "Vec2::is_zero")]
    pub out_tangent: Vec2,
    /// The end of the way here from the previous keyframe's value, relative
    /// to this value: that curve's second control point.
    #[serde(default, skip_serializing_if = "Vec2::is_zero")]
    pub in_tangent: Vec2,
}

/// One timestamped control value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", schemars(rename = "Keyframe_{T}"))]
pub struct Keyframe<T> {
    /// Timestamp of this value: timeline time, except in a composition
    /// node's settings, which are keyed in composition time, and in a
    /// shapes or layout generator's tree, which is keyed in its picture's
    /// time.
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
///
/// Each field a document leaves out takes its default: the
/// source centred on the canvas, unscaled, unrotated and opaque.
pub struct VisualProperties {
    /// Animated normalized canvas translation, along spatial curves where its
    /// keyframes have tangents.
    #[serde(default = "centre_point")]
    pub position: AnimatedPoint,
    /// Unanimated normalized point within the source rectangle.
    #[serde(default = "centre")]
    pub anchor: Vec2,
    /// Animated per-axis scale.
    #[serde(default = "unit_scale")]
    pub scale: Animated<Vec2>,
    /// Animated clockwise rotation in degrees.
    #[serde(default = "no_rotation")]
    pub rotation_degrees: Animated<f32>,
    /// Animated opacity constrained to `0..=1`.
    #[serde(default = "opaque")]
    pub opacity: Animated<f32>,
}

impl Default for VisualProperties {
    fn default() -> Self {
        Self {
            position: centre_point(),
            anchor: centre(),
            scale: unit_scale(),
            rotation_degrees: no_rotation(),
            opacity: opaque(),
        }
    }
}

const fn centre() -> Vec2 {
    Vec2::new(0.5, 0.5)
}

const fn centre_point() -> AnimatedPoint {
    AnimatedPoint::Constant { value: centre() }
}

/// A constant scale of 1 on both axes.
pub(crate) const fn unit_scale() -> Animated<Vec2> {
    Animated::Constant {
        value: Vec2::new(1.0, 1.0),
    }
}

const fn no_rotation() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
}

/// A constant opacity of 1.
pub(crate) const fn opaque() -> Animated<f32> {
    Animated::Constant { value: 1.0 }
}

/// Calls `visit` with `at` and the times of `animated`'s keyframes, when it
/// has them.
pub(crate) fn visit_keyed<T>(
    at: &str,
    animated: &Animated<T>,
    visit: &mut impl FnMut(&str, &[alpha2_time::MediaTime]),
) {
    if let Animated::Keyframes { keyframes } = animated {
        let times = keyframes
            .iter()
            .map(|keyframe| keyframe.time)
            .collect::<Vec<_>>();
        visit(at, &times);
    }
}

/// [`visit_keyed`] for a point along a spatial curve.
pub(crate) fn visit_keyed_point(
    at: &str,
    animated: &AnimatedPoint,
    visit: &mut impl FnMut(&str, &[alpha2_time::MediaTime]),
) {
    if let AnimatedPoint::Keyframes { keyframes } = animated {
        let times = keyframes
            .iter()
            .map(|keyframe| keyframe.time)
            .collect::<Vec<_>>();
        visit(at, &times);
    }
}

impl VisualProperties {
    pub(crate) fn validate(&self) -> bool {
        self.anchor.is_finite()
            && valid_point_animation(&self.position)
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

/// Valid keyframes (or a constant) whose values and tangents are finite.
pub(crate) fn valid_point_animation(animation: &AnimatedPoint) -> bool {
    match animation {
        AnimatedPoint::Constant { value } => value.is_finite(),
        AnimatedPoint::Keyframes { keyframes } => {
            !keyframes.is_empty()
                && keyframes.windows(2).all(|pair| pair[0].time < pair[1].time)
                && keyframes.iter().all(|keyframe| {
                    keyframe.interpolation.is_valid()
                        && keyframe.value.is_finite()
                        && keyframe.out_tangent.is_finite()
                        && keyframe.in_tangent.is_finite()
                })
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

/// A non-empty, strictly ascending sequence whose interpolations are usable.
pub(crate) fn valid_keyframes<T>(keyframes: &[Keyframe<T>]) -> bool {
    !keyframes.is_empty()
        && keyframes.windows(2).all(|pair| pair[0].time < pair[1].time)
        && keyframes
            .iter()
            .all(|keyframe| keyframe.interpolation.is_valid())
}
