use std::collections::BTreeMap;

use crate::{Animated, EffectResourceId, Interpolation};

use serde::{Deserialize, Serialize};

/// Artistic blend operation stored independently from a renderer implementation.
///
/// The modes are those of W3C Compositing and Blending Level 1 (the CSS
/// `mix-blend-mode` values), with the specification's formulas.
#[derive(Copy, Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "snake_case")]
pub enum ClipBlendMode {
    /// Preserve source color and apply source-over coverage.
    #[default]
    Normal,
    /// Multiply source and backdrop components.
    Multiply,
    /// Inverse-multiply source and backdrop components.
    Screen,
    /// Multiply dark backdrop regions and screen light regions.
    Overlay,
    /// The darker of source and backdrop, per channel.
    Darken,
    /// The lighter of source and backdrop, per channel.
    Lighten,
    /// Brighten the backdrop to reflect the source.
    ColorDodge,
    /// Darken the backdrop to reflect the source.
    ColorBurn,
    /// Multiply or screen, depending on the source.
    HardLight,
    /// Darken or lighten, depending on the source: a diffused hard light.
    SoftLight,
    /// The absolute difference of source and backdrop.
    Difference,
    /// Like difference, with lower contrast.
    Exclusion,
    /// The source's hue with the backdrop's saturation and luminosity.
    Hue,
    /// The source's saturation with the backdrop's hue and luminosity.
    Saturation,
    /// The source's hue and saturation with the backdrop's luminosity.
    Color,
    /// The source's luminosity with the backdrop's hue and saturation.
    Luminosity,
}

/// Serializable value supplied to one runtime effect parameter.
#[derive(Copy, Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum EffectValue {
    /// One finite scalar.
    Float(f32),
    /// Two finite components, normally in normalized coordinates.
    Vec2([f32; 2]),
    /// Four finite components that are not a color, such as per-channel
    /// thresholds; they keyframe component by component.
    Vec4([f32; 4]),
    /// Straight (not premultiplied), sRGB-encoded RGBA components in the
    /// inclusive unit interval, as a color picker shows them.
    Color([f32; 4]),
    /// Index into an effect definition's enum choices.
    Enum(u32),
    /// Boolean toggle.
    Bool(bool),
}

impl EffectValue {
    /// Whether keyframes of this type interpolate. `Enum` and `Bool` values
    /// have nothing between them, so their keyframes only hold.
    #[must_use]
    pub const fn interpolates(&self) -> bool {
        matches!(
            self,
            Self::Float(_) | Self::Vec2(_) | Self::Vec4(_) | Self::Color(_)
        )
    }

    /// Whether `other` is the same type of value.
    #[must_use]
    pub fn same_type(&self, other: &Self) -> bool {
        core::mem::discriminant(self) == core::mem::discriminant(other)
    }

    pub(crate) fn is_valid(&self) -> bool {
        match self {
            Self::Float(value) => value.is_finite(),
            Self::Vec2(values) => values.iter().all(|value| value.is_finite()),
            Self::Vec4(values) => values.iter().all(|value| value.is_finite()),
            Self::Color(values) => values
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)),
            Self::Enum(_) | Self::Bool(_) => true,
        }
    }
}

/// One versioned runtime effect invocation: a composition's effect or
/// transition node, or a timeline transition.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(deny_unknown_fields)]
pub struct ClipEffect {
    /// Stable effect definition identifier.
    pub id: String,
    /// Exact effect definition version required by the project.
    pub version: u32,
    /// Named parameter values, constant or keyframed. Omitted entries use
    /// definition defaults.
    ///
    /// Keyframe times are composition times in a composition node and
    /// timeline times in a timeline transition. A parameter's keyframes all
    /// have one value type, and `enum` and `bool` keyframes only hold. An
    /// interpolated value that a timing curve carries past the parameter's
    /// range is clamped to it; an authored value outside it is an error.
    #[serde(default)]
    pub params: BTreeMap<String, Animated<EffectValue>>,
    /// Named external resources matched against the effect definition.
    #[serde(default)]
    pub resources: BTreeMap<String, EffectResourceId>,
}

impl ClipEffect {
    pub(crate) fn is_valid(&self) -> bool {
        !self.id.is_empty()
            && self.version > 0
            && self
                .params
                .iter()
                .all(|(name, value)| valid_binding_name(name) && valid_param(value))
            && self
                .resources
                .iter()
                .all(|(name, id)| valid_binding_name(name) && !id.as_str().is_empty())
    }
}

/// Whether `name` can name what an effect binds: a parameter, a resource, or
/// a named input. An ASCII letter or `_`, then letters, digits, or `_`.
pub(crate) fn valid_binding_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

pub(crate) fn valid_param(animation: &Animated<EffectValue>) -> bool {
    match animation {
        Animated::Constant { value } => value.is_valid(),
        Animated::Keyframes { keyframes } => {
            let Some(first) = keyframes.first() else {
                return false;
            };
            crate::animation::valid_keyframes(keyframes)
                && keyframes.iter().all(|keyframe| {
                    keyframe.value.is_valid()
                        && keyframe.value.same_type(&first.value)
                        && (first.value.interpolates()
                            || keyframe.interpolation == Interpolation::Hold)
                })
        }
    }
}
