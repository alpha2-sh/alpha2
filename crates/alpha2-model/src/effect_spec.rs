//! The built-in effects and transitions every engine registers, as data.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use serde::Deserialize;

use crate::{Animated, ClipEffect, EffectValue, NodeId};

/// Id prefix of built-in effects and transitions. A document id under it must name a built-in, so
/// host effects use ids outside it.
pub const BUILTIN_EFFECT_PREFIX: &str = "alpha2.";

/// The catalog the renderer's built-ins serialize to, written by `emit-effect-catalog`.
const CATALOG_JSON: &str = include_str!("builtin-effects.json");

/// What an effect definition draws.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    /// An image effect with one source and any declared named inputs.
    Effect,
    /// A two-input transition at an explicit progress.
    Transition,
}

/// The type of a parameter's values: the [`EffectValue`] variant it takes.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EffectParamType {
    /// [`EffectValue::Float`].
    Float,
    /// [`EffectValue::Vec2`].
    Vec2,
    /// [`EffectValue::Vec4`].
    Vec4,
    /// [`EffectValue::Color`].
    Color,
    /// [`EffectValue::Enum`].
    Enum,
    /// [`EffectValue::Bool`].
    Bool,
}

/// How a parameter's value reads in an editor.
#[derive(Copy, Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum EffectParamUnit {
    /// A plain or normalized number.
    Normalized,
    /// An angle in degrees.
    Degrees,
    /// A color.
    Color,
    /// One of the named choices.
    Enum,
    /// A toggle.
    Bool,
}

/// One declared parameter of a built-in definition.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct EffectParamSpec {
    /// The name a [`ClipEffect`] sets it by.
    pub name: String,
    /// A human-readable label.
    pub label: String,
    /// The type of its values.
    pub param_type: EffectParamType,
    /// The value used when a document leaves it out.
    pub default: EffectValue,
    /// Inclusive minimum of each scalar component.
    pub min: Option<f32>,
    /// Inclusive maximum of each scalar component.
    pub max: Option<f32>,
    /// A suggested editing increment.
    pub step: Option<f32>,
    /// How its value reads.
    pub unit: EffectParamUnit,
    /// The choices of an [`EffectParamType::Enum`]; an enum value is an index into them.
    pub enum_values: Vec<String>,
}

impl EffectParamSpec {
    /// Whether `value` has this parameter's type and lies in its range, as the renderer
    /// requires of an authored value.
    #[must_use]
    pub fn accepts(&self, value: &EffectValue) -> bool {
        let scalar = |value: f32| {
            value.is_finite()
                && self.min.is_none_or(|minimum| value >= minimum)
                && self.max.is_none_or(|maximum| value <= maximum)
        };
        match (self.param_type, value) {
            (EffectParamType::Float, EffectValue::Float(value)) => scalar(*value),
            (EffectParamType::Vec2, EffectValue::Vec2(values)) => {
                values.iter().all(|value| scalar(*value))
            }
            (EffectParamType::Vec4, EffectValue::Vec4(values)) => {
                values.iter().all(|value| scalar(*value))
            }
            (EffectParamType::Color, EffectValue::Color(values)) => values
                .iter()
                .all(|value| value.is_finite() && (0.0..=1.0).contains(value)),
            (EffectParamType::Enum, EffectValue::Enum(index)) => {
                usize::try_from(*index).is_ok_and(|index| index < self.enum_values.len())
            }
            (EffectParamType::Bool, EffectValue::Bool(_)) => true,
            _ => false,
        }
    }

    /// Whether the parameter can follow a data set, which only Float and Vec2 ones can.
    #[must_use]
    pub const fn takes_data(&self) -> bool {
        matches!(
            self.param_type,
            EffectParamType::Float | EffectParamType::Vec2
        )
    }

    /// Whether every authored value of `animation` is accepted, or for a data binding whether the
    /// type takes data. Only keyframe values count, since interpolated overshoot is clamped.
    #[must_use]
    pub fn accepts_animation(&self, animation: &Animated<EffectValue>) -> bool {
        match animation {
            Animated::Constant { value } => self.accepts(value),
            Animated::Keyframes { keyframes } => keyframes
                .iter()
                .all(|keyframe| self.accepts(&keyframe.value)),
            Animated::Data(_) => self.takes_data(),
        }
    }
}

/// A built-in effect or transition definition, without its shader.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub struct EffectSpec {
    /// The id documents name it by.
    pub id: String,
    /// The version documents must name.
    pub version: u32,
    /// What it draws.
    pub kind: EffectKind,
    /// A human-readable label.
    pub label: String,
    /// The catalog group it is listed under.
    pub group: String,
    /// The named images it samples besides its input.
    pub inputs: Vec<String>,
    /// Its parameters, in declaration order.
    pub params: Vec<EffectParamSpec>,
    /// The external resources it reads, by slot name.
    pub resources: Vec<String>,
    /// Whether this transition of two pictures under one coverage (a matte or mask) equals that
    /// coverage over their transition, for every parameter value.
    pub coverage_commutes: bool,
}

impl EffectSpec {
    /// The parameter named `name`.
    #[must_use]
    pub fn param(&self, name: &str) -> Option<&EffectParamSpec> {
        self.params.iter().find(|param| param.name == name)
    }
}

/// Every built-in definition: effects, then transitions, in catalog order.
#[must_use]
pub fn builtin_effects() -> &'static [EffectSpec] {
    &catalog().0
}

/// The built-in definition with `id`.
#[must_use]
pub fn builtin_effect(id: &str) -> Option<&'static EffectSpec> {
    let (specs, index) = catalog();
    index.get(id).map(|position| &specs[*position])
}

/// The catalog JSON the built-ins were read from, as `schema/effects.v1.json` publishes it.
#[must_use]
pub const fn builtin_effects_json() -> &'static str {
    CATALOG_JSON
}

fn catalog() -> &'static (Vec<EffectSpec>, HashMap<String, usize>) {
    static CATALOG: OnceLock<(Vec<EffectSpec>, HashMap<String, usize>)> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let wire: CatalogWire =
            serde_json::from_str(CATALOG_JSON).expect("the built-in effect catalog parses");
        let specs = wire
            .effects
            .into_iter()
            .chain(wire.transitions)
            .map(EffectSpec::from)
            .collect::<Vec<_>>();
        let index = specs
            .iter()
            .enumerate()
            .map(|(position, spec)| (spec.id.clone(), position))
            .collect();
        (specs, index)
    })
}

/// Why a [`ClipEffect`] naming a built-in does not match it.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum EffectProblem {
    /// The id is under [`BUILTIN_EFFECT_PREFIX`] but names no built-in.
    UnknownBuiltin,
    /// The version is not the built-in's.
    Version {
        /// The built-in's version.
        expected: u32,
    },
    /// A transition where an effect belongs, or an effect where a transition belongs.
    Kind,
    /// A parameter the definition does not declare.
    UnknownParam(String),
    /// A parameter value of the wrong type or outside the declared range.
    InvalidParam(String),
    /// Named inputs other than exactly the declared ones.
    Inputs,
    /// Resources other than exactly the declared ones.
    Resources,
}

/// Checks `effect` against its built-in definition, used as `kind`; `inputs` are an effect
/// node's named inputs. Ids outside [`BUILTIN_EFFECT_PREFIX`] belong to the host and pass.
pub(crate) fn check_builtin(
    effect: &ClipEffect,
    kind: EffectKind,
    inputs: Option<&BTreeMap<String, Option<NodeId>>>,
) -> Result<(), EffectProblem> {
    if !effect.id.starts_with(BUILTIN_EFFECT_PREFIX) {
        return Ok(());
    }
    let spec = builtin_effect(&effect.id).ok_or(EffectProblem::UnknownBuiltin)?;
    if effect.version != spec.version {
        return Err(EffectProblem::Version {
            expected: spec.version,
        });
    }
    if spec.kind != kind {
        return Err(EffectProblem::Kind);
    }
    for (name, value) in &effect.params {
        let param = spec
            .param(name)
            .ok_or_else(|| EffectProblem::UnknownParam(name.clone()))?;
        if !param.accepts_animation(value) {
            return Err(EffectProblem::InvalidParam(name.clone()));
        }
    }
    let declared = |names: &[String], used: &mut dyn Iterator<Item = &String>| {
        let used = used.collect::<Vec<_>>();
        used.len() == names.len() && used.iter().all(|name| names.contains(name))
    };
    if !declared(
        &spec.inputs,
        &mut inputs.into_iter().flat_map(BTreeMap::keys),
    ) {
        return Err(EffectProblem::Inputs);
    }
    if !declared(&spec.resources, &mut effect.resources.keys()) {
        return Err(EffectProblem::Resources);
    }
    Ok(())
}

#[derive(Deserialize)]
struct CatalogWire {
    effects: Vec<SpecWire>,
    transitions: Vec<SpecWire>,
}

#[derive(Deserialize)]
struct SpecWire {
    id: String,
    version: u32,
    kind: EffectKind,
    label: String,
    group: String,
    inputs: Vec<NamedWire>,
    params: Vec<ParamWire>,
    resources: Vec<NamedWire>,
    #[serde(default)]
    coverage_commutes: bool,
}

#[derive(Deserialize)]
struct NamedWire {
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParamWire {
    name: String,
    label: String,
    #[serde(rename = "type")]
    param_type: EffectParamType,
    default: serde_json::Value,
    min: Option<f32>,
    max: Option<f32>,
    step: Option<f32>,
    unit: EffectParamUnit,
    enum_values: Vec<String>,
}

impl From<SpecWire> for EffectSpec {
    fn from(wire: SpecWire) -> Self {
        Self {
            id: wire.id,
            version: wire.version,
            kind: wire.kind,
            label: wire.label,
            group: wire.group,
            inputs: wire.inputs.into_iter().map(|input| input.name).collect(),
            params: wire.params.into_iter().map(EffectParamSpec::from).collect(),
            resources: wire
                .resources
                .into_iter()
                .map(|resource| resource.name)
                .collect(),
            coverage_commutes: wire.coverage_commutes,
        }
    }
}

impl From<ParamWire> for EffectParamSpec {
    fn from(wire: ParamWire) -> Self {
        let default = default_value(wire.param_type, &wire.default)
            .unwrap_or_else(|| panic!("parameter {} has a default of its type", wire.name));
        Self {
            name: wire.name,
            label: wire.label,
            param_type: wire.param_type,
            default,
            min: wire.min,
            max: wire.max,
            step: wire.step,
            unit: wire.unit,
            enum_values: wire.enum_values,
        }
    }
}

#[allow(clippy::cast_possible_truncation)]
fn default_value(param_type: EffectParamType, value: &serde_json::Value) -> Option<EffectValue> {
    // The catalog widens each `f32` to `f64`; narrowing restores it exactly.
    let numbers = |count: usize| {
        let values = value.as_array()?;
        (values.len() == count)
            .then(|| {
                values
                    .iter()
                    .map(|value| value.as_f64().map(|value| value as f32))
            })?
            .collect::<Option<Vec<f32>>>()
    };
    Some(match param_type {
        EffectParamType::Float => EffectValue::Float(value.as_f64()? as f32),
        EffectParamType::Vec2 => EffectValue::Vec2(numbers(2)?.try_into().ok()?),
        EffectParamType::Vec4 => EffectValue::Vec4(numbers(4)?.try_into().ok()?),
        EffectParamType::Color => EffectValue::Color(numbers(4)?.try_into().ok()?),
        EffectParamType::Enum => EffectValue::Enum(u32::try_from(value.as_u64()?).ok()?),
        EffectParamType::Bool => EffectValue::Bool(value.as_bool()?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_parses_with_defaults_it_accepts() {
        let specs = builtin_effects();
        assert!(specs.iter().any(|spec| spec.kind == EffectKind::Effect));
        assert!(specs.iter().any(|spec| spec.kind == EffectKind::Transition));
        for spec in specs {
            assert!(spec.id.starts_with(BUILTIN_EFFECT_PREFIX), "{}", spec.id);
            assert_eq!(builtin_effect(&spec.id), Some(spec));
            for param in &spec.params {
                assert!(param.accepts(&param.default), "{} {}", spec.id, param.name);
            }
        }
    }
}
