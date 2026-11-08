use crate::{CubicBezier, DataBinding, Easing};
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

/// A value that is constant, controlled by sorted keyframes, or following a data set.
///
/// Only value types whose [`AnimatedValue::ACCEPTS_DATA`] is true (`f32`, [`Vec2`] and
/// [`crate::EffectValue`]) read or offer the `data` mode; for the others it fails to
/// deserialize and the schema leaves it out.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
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
    /// A channel of a data set, followed at render time.
    Data(Box<DataBinding>),
}

mod sealed {
    pub trait Sealed {}
}

/// A type an [`Animated`] value can hold. Sealed: implemented for the model's animatable types.
pub trait AnimatedValue: sealed::Sealed {
    /// Whether the type takes the `data` value mode.
    const ACCEPTS_DATA: bool;
}

macro_rules! animated_value {
    ($accepts:literal: $($type:ty),+) => {
        $(
            impl sealed::Sealed for $type {}
            impl AnimatedValue for $type {
                const ACCEPTS_DATA: bool = $accepts;
            }
        )+
    };
}

animated_value!(true: f32, Vec2, crate::EffectValue);
animated_value!(
    false: String,
    crate::Paint,
    crate::Rgba,
    crate::Path,
    crate::LayoutLength,
    crate::LayoutSize,
    crate::LayoutOffset,
    crate::Insets
);

// The wire form of `Animated`, before the value type decides whether it takes data. Its doc
// comment is the schema's description.
/// A value that is constant, controlled by sorted keyframes, or following a data set.
#[derive(Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
#[cfg_attr(feature = "schema", schemars(rename = "Animated_{T}"))]
enum AnimatedWire<T> {
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
    /// A channel of a data set, followed at render time.
    Data(DataBinding),
}

impl<'de, T: Deserialize<'de> + AnimatedValue> Deserialize<'de> for Animated<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match AnimatedWire::<T>::deserialize(deserializer)? {
            AnimatedWire::Constant { value } => Self::Constant { value },
            AnimatedWire::Keyframes { keyframes } => Self::Keyframes { keyframes },
            AnimatedWire::Data(binding) if T::ACCEPTS_DATA => Self::Data(Box::new(binding)),
            AnimatedWire::Data(_) => {
                return Err(serde::de::Error::custom(
                    "this value takes no `data` mode; use `constant` or `keyframes`",
                ));
            }
        })
    }
}

#[cfg(feature = "schema")]
impl<T: schemars::JsonSchema + AnimatedValue> schemars::JsonSchema for Animated<T> {
    // Types without the data mode are a separate generic, `Keyframed_{T}`, so each generic has
    // one shape.
    fn schema_name() -> std::borrow::Cow<'static, str> {
        if T::ACCEPTS_DATA {
            AnimatedWire::<T>::schema_name()
        } else {
            format!("Keyframed_{}", T::schema_name()).into()
        }
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        if T::ACCEPTS_DATA {
            AnimatedWire::<T>::schema_id()
        } else {
            format!("alpha2_model::Keyframed<{}>", T::schema_id()).into()
        }
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        let mut schema = AnimatedWire::<T>::json_schema(generator);
        if !T::ACCEPTS_DATA {
            crate::schema::drop_data_mode(&mut schema);
            schema.insert(
                "description".to_owned(),
                "A value that is constant or controlled by sorted keyframes.".into(),
            );
        }
        schema
    }
}

impl<T> Animated<T> {
    /// The same animation with each value mapped by `map`, keeping times and interpolations.
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
            Self::Data(binding) => Animated::Data(binding.clone()),
        }
    }

    /// The data binding, when the value follows a data set.
    #[must_use]
    pub const fn data(&self) -> Option<&DataBinding> {
        match self {
            Self::Data(binding) => Some(binding),
            Self::Constant { .. } | Self::Keyframes { .. } => None,
        }
    }

    /// Whether the value can change over time: keyframes or data.
    #[must_use]
    pub const fn varies(&self) -> bool {
        !matches!(self, Self::Constant { .. })
    }
}

/// A position that can move along curves between its keyframes.
///
/// Its JSON is an `Animated<Vec2>` plus optional spatial tangents per keyframe. With them,
/// the value travels the cubic Bezier through this keyframe's `out_tangent` and the next's
/// `in_tangent`, at the speed its timing curve sets along the arc length. Without them it
/// moves in a straight line.
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
    /// A channel of a data set, followed at render time.
    Data(Box<DataBinding>),
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
            Animated::Data(binding) => Self::Data(binding),
        }
    }
}

impl AnimatedPoint {
    /// Whether the value changes over time: keyframes or data.
    #[must_use]
    pub const fn is_animated(&self) -> bool {
        !matches!(self, Self::Constant { .. })
    }

    /// The data binding, when the value follows a data set.
    #[must_use]
    pub const fn data(&self) -> Option<&DataBinding> {
        match self {
            Self::Data(binding) => Some(binding),
            Self::Constant { .. } | Self::Keyframes { .. } => None,
        }
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
    /// Interpolation from this keyframe to its successor: how fast the value travels along the
    /// way.
    pub interpolation: Interpolation,
    /// The curve's first control point toward the next keyframe, relative to this value.
    #[serde(default, skip_serializing_if = "Vec2::is_zero")]
    pub out_tangent: Vec2,
    /// The curve's second control point from the previous keyframe, relative to this value.
    #[serde(default, skip_serializing_if = "Vec2::is_zero")]
    pub in_tangent: Vec2,
}

/// One timestamped control value.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[cfg_attr(feature = "schema", schemars(rename = "Keyframe_{T}"))]
pub struct Keyframe<T> {
    /// Timestamp of this value. Timeline time, except composition time in a composition node's
    /// settings and picture time in a shapes or layout generator's tree.
    pub time: alpha2_time::MediaTime,
    /// Value at `time`.
    pub value: T,
    /// Interpolation from this keyframe to its successor.
    pub interpolation: Interpolation,
}

/// Interpolation from one keyframe to the next.
///
/// Values interpolate numerically: a rotation from 0 to 720 degrees turns twice. A host that
/// wants the shortest path normalizes its keyframe values.
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

/// The wire form of [`Interpolation`]; see `EasingWire` for why unit variants are empty
/// structs.
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
    /// The fraction of the way to the next value at linear segment `progress` (clamped to
    /// `0..=1`), or `None` while holding. A cubic Bezier may overshoot `0..=1`.
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
/// Omitted fields default to the source centred on the canvas, unscaled, unrotated and opaque.
pub struct VisualProperties {
    /// Animated normalized canvas translation, along spatial curves where keyframes have
    /// tangents.
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
    /// A `similarity` data set moving the clip's own picture, applied before this transform.
    /// Clip visuals only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<crate::CorrectionRef>,
}

impl Default for VisualProperties {
    fn default() -> Self {
        Self {
            position: centre_point(),
            anchor: centre(),
            scale: unit_scale(),
            rotation_degrees: no_rotation(),
            opacity: opaque(),
            correction: None,
        }
    }
}

const fn centre() -> Vec2 {
    Vec2::new(0.5, 0.5)
}

const fn centre_point() -> AnimatedPoint {
    AnimatedPoint::Constant { value: centre() }
}

pub(crate) const fn unit_scale() -> Animated<Vec2> {
    Animated::Constant {
        value: Vec2::new(1.0, 1.0),
    }
}

const fn no_rotation() -> Animated<f32> {
    Animated::Constant { value: 0.0 }
}

pub(crate) const fn opaque() -> Animated<f32> {
    Animated::Constant { value: 1.0 }
}

/// What a walk over animated values meets at one value that is not constant.
pub(crate) enum Keyed<'a> {
    /// The times of its keyframes.
    Times(&'a [alpha2_time::MediaTime]),
    /// Its data binding, with the type of the value bound.
    Data(&'a DataBinding, PlaceType),
}

/// The value type a data binding sits on, which decides the kinds it can follow.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum PlaceType {
    /// A point along spatial curves.
    Point,
    /// A two-component value.
    Vec2,
    /// A number.
    Scalar,
    /// An effect parameter, whose type its definition gives.
    Effect,
    /// A type that takes no data.
    Other,
}

/// The [`PlaceType`] of each animatable type.
pub(crate) trait Placed {
    const PLACE: PlaceType;
}

macro_rules! placed {
    ($place:ident: $($type:ty),+) => {
        $(impl Placed for $type {
            const PLACE: PlaceType = PlaceType::$place;
        })+
    };
}

placed!(Scalar: f32);
placed!(Vec2: Vec2);
placed!(Effect: crate::EffectValue);
placed!(
    Other: String,
    crate::Paint,
    crate::Rgba,
    crate::Path,
    crate::LayoutLength,
    crate::LayoutSize,
    crate::LayoutOffset,
    crate::Insets
);

/// Calls `visit` with `at` and the times of `animated`'s keyframes, or its data binding.
pub(crate) fn visit_keyed<T: Placed>(
    at: &str,
    animated: &Animated<T>,
    visit: &mut impl FnMut(&str, Keyed<'_>),
) {
    match animated {
        Animated::Keyframes { keyframes } => {
            let times = keyframes
                .iter()
                .map(|keyframe| keyframe.time)
                .collect::<Vec<_>>();
            visit(at, Keyed::Times(&times));
        }
        Animated::Data(binding) => visit(at, Keyed::Data(binding, T::PLACE)),
        Animated::Constant { .. } => {}
    }
}

/// [`visit_keyed`] for a point along a spatial curve.
pub(crate) fn visit_keyed_point(
    at: &str,
    animated: &AnimatedPoint,
    visit: &mut impl FnMut(&str, Keyed<'_>),
) {
    match animated {
        AnimatedPoint::Keyframes { keyframes } => {
            let times = keyframes
                .iter()
                .map(|keyframe| keyframe.time)
                .collect::<Vec<_>>();
            visit(at, Keyed::Times(&times));
        }
        AnimatedPoint::Data(binding) => visit(at, Keyed::Data(binding, PlaceType::Point)),
        AnimatedPoint::Constant { .. } => {}
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
        Animated::Data(_) => true,
    }
}

/// Valid keyframes (or a constant) whose values and tangents are finite.
pub(crate) fn valid_point_animation(animation: &AnimatedPoint) -> bool {
    match animation {
        AnimatedPoint::Constant { value } => value.is_finite(),
        AnimatedPoint::Data(_) => true,
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
        Animated::Data(_) => true,
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
        Animated::Data(_) => true,
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
