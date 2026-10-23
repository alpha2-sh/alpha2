//! Deserialized runtime effect and parameter definitions.

use alpha2_gpu::CapabilityTier;
use alpha2_render::{
    EffectDef, EffectKind, EffectResourceKind, InputSpec, ParamSpec, ParamType, ParamUnit,
    ParamValue, ResourceSpec,
};
use serde::Deserialize;
use serde_json::Value;

/// A runtime effect as the protocol sends it (camelCase JSON), converted to the renderer's
/// [`EffectDef`] by [`EffectRequest::into_definition`].
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectRequest {
    pub id: String,
    pub version: u32,
    pub kind: EffectKindRequest,
    /// The images an effect samples besides its source, by name.
    #[serde(default)]
    pub inputs: Vec<InputRequest>,
    #[serde(default)]
    pub params: Vec<ParameterRequest>,
    #[serde(default)]
    pub resources: Vec<ResourceRequest>,
    pub wgsl_body: String,
    /// The lowest capability tier the effect runs on. Defaults to tier B when omitted.
    #[serde(default = "default_tier")]
    pub tier_min: TierRequest,
    #[serde(default)]
    pub license: Option<String>,
}

/// One named input of an effect as the protocol sends it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct InputRequest {
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequest {
    pub name: String,
    pub kind: ResourceKindRequest,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ResourceKindRequest {
    Lut3d,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EffectKindRequest {
    Effect,
    Transition,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TierRequest {
    A,
    B,
}

/// One effect parameter as the protocol sends it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParameterRequest {
    pub name: String,
    #[serde(rename = "type")]
    pub param_type: ParameterTypeRequest,
    /// A number for `float`, an array of 2 or 4 numbers for `vec2`/`vec4`/`color`, a
    /// non-negative index for `enum`, and a boolean for `bool`.
    pub default: Value,
    #[serde(default)]
    pub min: Option<f32>,
    #[serde(default)]
    pub max: Option<f32>,
    #[serde(default)]
    pub step: Option<f32>,
    pub unit: ParameterUnitRequest,
    #[serde(default)]
    pub enum_values: Vec<String>,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterTypeRequest {
    Float,
    Vec2,
    Vec4,
    Color,
    Enum,
    Bool,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ParameterUnitRequest {
    Normalized,
    Degrees,
    Color,
    Enum,
    Bool,
}

impl EffectRequest {
    /// Converts the request into the renderer's definition.
    ///
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
    /// Converts the parameter into the renderer's spec.
    ///
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

/// Reads `N` floats from a parameter default. A scalar is accepted when `N` is 1; otherwise
/// `json` must be an array of exactly `N` numbers.
///
/// # Errors
/// Names the parameter `name` when the default has the wrong shape or a non-numeric element.
pub fn value<const N: usize>(json: &Value, name: &str) -> Result<[f32; N], String> {
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

/// The tier assumed when a request omits `tierMin`.
pub fn default_tier() -> TierRequest {
    TierRequest::B
}
