//! A runtime effect a request carries.

use alpha2_gpu::CapabilityTier;
use alpha2_render::{
    EffectDef, EffectKind, EffectResourceKind, InputSpec, ParamSpec, ParamType, ParamUnit,
    ParamValue, ResourceSpec,
};
use serde::Deserialize;
use serde_json::Value;

/// A runtime effect as the protocol sends it (camelCase JSON).
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EffectRequest {
    /// The effect's id, which clips name.
    pub id: String,
    /// Bumped when the effect's output changes.
    pub version: u32,
    /// An effect or a transition.
    pub kind: EffectKindRequest,
    /// Images the effect samples besides its source, by name.
    #[serde(default)]
    pub inputs: Vec<InputRequest>,
    /// The parameters clips set, in order.
    #[serde(default)]
    pub params: Vec<ParameterRequest>,
    /// Tables the effect samples, by name.
    #[serde(default)]
    pub resources: Vec<ResourceRequest>,
    /// The WGSL body the engine wraps into a shader.
    pub wgsl_body: String,
    /// Defaults to tier B when omitted.
    #[serde(default = "default_tier")]
    pub tier_min: TierRequest,
    /// The license the author ships the effect under.
    #[serde(default)]
    pub license: Option<String>,
}

/// An image an effect samples besides its source.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputRequest {
    /// The name the body samples it by.
    pub name: String,
}

/// A table an effect samples.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequest {
    /// The name the body samples it by.
    pub name: String,
    /// What the table is.
    pub kind: ResourceKindRequest,
}

/// The kinds of table an effect samples.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceKindRequest {
    /// A 3D lookup table.
    Lut3d,
}

/// Whether the effect draws one clip or the cut between two.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectKindRequest {
    /// Draws one clip.
    Effect,
    /// Draws the cut between two clips.
    Transition,
}

/// The least capable GPU tier the effect runs on.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TierRequest {
    /// Tier A.
    A,
    /// Tier B.
    B,
}

/// A parameter of a runtime effect.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterRequest {
    /// The name the body reads it by.
    pub name: String,
    /// The value's shape.
    #[serde(rename = "type")]
    pub param_type: ParameterTypeRequest,
    /// A number for `float`, an array of 2 or 4 numbers for `vec2`/`vec4`/`color`, a
    /// non-negative index for `enum`, and a boolean for `bool`.
    pub default: Value,
    /// The least value an editor offers.
    #[serde(default)]
    pub min: Option<f32>,
    /// The greatest value an editor offers.
    #[serde(default)]
    pub max: Option<f32>,
    /// The step an editor moves by.
    #[serde(default)]
    pub step: Option<f32>,
    /// How an editor shows the value.
    pub unit: ParameterUnitRequest,
    /// The names of an `enum` parameter's values, by index.
    #[serde(default)]
    pub enum_values: Vec<String>,
}

/// The shape of a parameter's value.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterTypeRequest {
    /// One number.
    Float,
    /// Two numbers.
    Vec2,
    /// Four numbers.
    Vec4,
    /// Linear RGBA.
    Color,
    /// An index into the parameter's names.
    Enum,
    /// True or false.
    Bool,
}

/// How an editor shows a parameter.
#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterUnitRequest {
    /// A number in `0..=1`.
    Normalized,
    /// An angle in degrees.
    Degrees,
    /// A color.
    Color,
    /// One of the parameter's names.
    Enum,
    /// True or false.
    Bool,
}

impl EffectRequest {
    /// # Errors
    /// Fails when a parameter's default does not match its type.
    pub fn into_definition(self) -> Result<EffectDef, String> {
        Ok(EffectDef {
            id: self.id,
            version: self.version,
            kind: match self.kind {
                EffectKindRequest::Effect => EffectKind::Effect,
                EffectKindRequest::Transition => EffectKind::Transition,
            },
            inputs: self
                .inputs
                .into_iter()
                .map(|input| InputSpec { name: input.name })
                .collect(),
            params: self
                .params
                .into_iter()
                .map(ParameterRequest::into_spec)
                .collect::<Result<_, _>>()?,
            resources: self
                .resources
                .into_iter()
                .map(|resource| ResourceSpec {
                    name: resource.name,
                    kind: match resource.kind {
                        ResourceKindRequest::Lut3d => EffectResourceKind::Lut3d,
                    },
                })
                .collect(),
            wgsl_body: self.wgsl_body,
            tier_min: match self.tier_min {
                TierRequest::A => CapabilityTier::TierA,
                TierRequest::B => CapabilityTier::TierB,
            },
            license: self.license,
        })
    }
}

impl ParameterRequest {
    /// # Errors
    /// Fails when the default does not match the parameter's type.
    pub fn into_spec(self) -> Result<ParamSpec, String> {
        let bad = || format!("parameter {} has an invalid default", self.name);
        let (param_type, default) = match self.param_type {
            ParameterTypeRequest::Float => (
                ParamType::Float,
                ParamValue::Float(value::<1>(&self.default, &self.name)?[0]),
            ),
            ParameterTypeRequest::Vec2 => (
                ParamType::Vec2,
                ParamValue::Vec2(value::<2>(&self.default, &self.name)?),
            ),
            ParameterTypeRequest::Vec4 => (
                ParamType::Vec4,
                ParamValue::Vec4(value::<4>(&self.default, &self.name)?),
            ),
            ParameterTypeRequest::Color => (
                ParamType::Color,
                ParamValue::Color(value::<4>(&self.default, &self.name)?),
            ),
            ParameterTypeRequest::Enum => (
                ParamType::Enum,
                ParamValue::Enum(
                    self.default
                        .as_u64()
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(bad)?,
                ),
            ),
            ParameterTypeRequest::Bool => (
                ParamType::Bool,
                ParamValue::Bool(self.default.as_bool().ok_or_else(bad)?),
            ),
        };
        Ok(ParamSpec {
            name: self.name,
            param_type,
            default,
            min: self.min,
            max: self.max,
            step: self.step,
            unit: match self.unit {
                ParameterUnitRequest::Normalized => ParamUnit::Normalized,
                ParameterUnitRequest::Degrees => ParamUnit::Degrees,
                ParameterUnitRequest::Color => ParamUnit::Color,
                ParameterUnitRequest::Enum => ParamUnit::Enum,
                ParameterUnitRequest::Bool => ParamUnit::Bool,
            },
            enum_values: self.enum_values,
        })
    }
}

/// A scalar is accepted when `N` is 1; otherwise `json` must be an array of exactly `N` numbers.
///
/// # Errors
/// Names the parameter when the default has the wrong shape or a non-numeric element.
fn value<const N: usize>(json: &Value, name: &str) -> Result<[f32; N], String> {
    let values: Vec<Value> = if N == 1 && !json.is_array() {
        vec![json.clone()]
    } else {
        json.as_array()
            .cloned()
            .ok_or_else(|| format!("parameter {name} has an invalid default"))?
    };
    if values.len() != N {
        return Err(format!("parameter {name} has an invalid default"));
    }
    let mut output = [0.0; N];
    for (target, source) in output.iter_mut().zip(values) {
        *target = serde_json::from_value(source)
            .map_err(|_| format!("parameter {name} has an invalid default"))?;
    }
    Ok(output)
}

fn default_tier() -> TierRequest {
    TierRequest::B
}
