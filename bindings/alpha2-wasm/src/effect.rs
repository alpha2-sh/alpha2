//! The JSON wire form of a custom effect definition, as the TypeScript package
//! sends it, and its conversion to the renderer's [`EffectDef`].

use alpha2_gpu::CapabilityTier;
use alpha2_render::{
    EffectDef, EffectKind, EffectResourceKind, InputSpec, ParamSpec, ParamType, ParamUnit,
    ParamValue, ResourceSpec,
};
use serde::Deserialize;

/// An effect definition in camelCase wire form. `tierMin` defaults to tier B;
/// `inputs`, `params` and `resources` default to empty.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EffectRequest {
    id: String,
    version: u32,
    kind: Kind,
    #[serde(default)]
    inputs: Vec<InputRequest>,
    #[serde(default)]
    params: Vec<ParameterRequest>,
    #[serde(default)]
    resources: Vec<ResourceRequest>,
    wgsl_body: String,
    #[serde(default = "default_tier")]
    tier_min: Tier,
    #[serde(default)]
    license: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InputRequest {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceRequest {
    name: String,
    kind: ResourceKind,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ResourceKind {
    Lut3d,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Kind {
    Effect,
    Transition,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Tier {
    A,
    B,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParameterRequest {
    name: String,
    #[serde(rename = "type")]
    param_type: ParameterType,
    default: serde_json::Value,
    #[serde(default)]
    min: Option<f32>,
    #[serde(default)]
    max: Option<f32>,
    #[serde(default)]
    step: Option<f32>,
    unit: ParameterUnit,
    #[serde(default)]
    enum_values: Vec<String>,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ParameterType {
    Float,
    Vec2,
    Vec4,
    Color,
    Enum,
    Bool,
}

#[derive(Copy, Clone, Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ParameterUnit {
    Normalized,
    Degrees,
    Color,
    Enum,
    Bool,
}

impl EffectRequest {
    /// Parses a wire definition into an [`EffectDef`].
    ///
    /// Fails with a message for JSON that does not match the wire schema, or a
    /// parameter default whose shape does not match its type. The WGSL body is not compiled here.
    pub(crate) fn parse(json: &str) -> Result<EffectDef, String> {
        serde_json::from_str::<Self>(json)
            .map_err(|error| error.to_string())?
            .into_definition()
    }

    fn into_definition(self) -> Result<EffectDef, String> {
        let kind = match self.kind {
            Kind::Effect => EffectKind::Effect,
            Kind::Transition => EffectKind::Transition,
        };
        let inputs = self
            .inputs
            .into_iter()
            .map(|input| InputSpec { name: input.name })
            .collect();
        let params = self
            .params
            .into_iter()
            .map(ParameterRequest::into_spec)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(EffectDef {
            id: self.id,
            version: self.version,
            kind,
            inputs,
            params,
            resources: self
                .resources
                .into_iter()
                .map(|resource| ResourceSpec {
                    name: resource.name,
                    kind: match resource.kind {
                        ResourceKind::Lut3d => EffectResourceKind::Lut3d,
                    },
                })
                .collect(),
            wgsl_body: self.wgsl_body,
            tier_min: match self.tier_min {
                Tier::A => CapabilityTier::TierA,
                Tier::B => CapabilityTier::TierB,
            },
            license: self.license,
        })
    }
}

impl ParameterRequest {
    fn into_spec(self) -> Result<ParamSpec, String> {
        let (param_type, default) = match self.param_type {
            ParameterType::Float => (
                ParamType::Float,
                ParamValue::Float(value(&self.default, &self.name)?),
            ),
            ParameterType::Vec2 => (
                ParamType::Vec2,
                ParamValue::Vec2(array(&self.default, &self.name)?),
            ),
            ParameterType::Vec4 => (
                ParamType::Vec4,
                ParamValue::Vec4(array(&self.default, &self.name)?),
            ),
            ParameterType::Color => (
                ParamType::Color,
                ParamValue::Color(array(&self.default, &self.name)?),
            ),
            ParameterType::Enum => (
                ParamType::Enum,
                ParamValue::Enum(
                    self.default
                        .as_u64()
                        .and_then(|value| u32::try_from(value).ok())
                        .ok_or_else(|| default_error(&self.name))?,
                ),
            ),
            ParameterType::Bool => (
                ParamType::Bool,
                ParamValue::Bool(
                    self.default
                        .as_bool()
                        .ok_or_else(|| default_error(&self.name))?,
                ),
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
                ParameterUnit::Normalized => ParamUnit::Normalized,
                ParameterUnit::Degrees => ParamUnit::Degrees,
                ParameterUnit::Color => ParamUnit::Color,
                ParameterUnit::Enum => ParamUnit::Enum,
                ParameterUnit::Bool => ParamUnit::Bool,
            },
            enum_values: self.enum_values,
        })
    }
}

fn default_tier() -> Tier {
    Tier::B
}

fn value(json: &serde_json::Value, name: &str) -> Result<f32, String> {
    serde_json::from_value::<f32>(json.clone()).map_err(|_| default_error(name))
}

fn array<const N: usize>(json: &serde_json::Value, name: &str) -> Result<[f32; N], String> {
    let values = json.as_array().ok_or_else(|| default_error(name))?;
    if values.len() != N {
        return Err(default_error(name));
    }
    let mut output = [0.0; N];
    for (output, input) in output.iter_mut().zip(values) {
        *output = value(input, name)?;
    }
    Ok(output)
}

fn default_error(name: &str) -> String {
    format!("parameter {name} has a default that does not match its type")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_typescript_wire_schema() {
        let definition = EffectRequest::parse(
            r#"{
                "id":"example.tint","version":1,"kind":"effect",
                "params":[{
                    "name":"tint","type":"color","default":[1,0.5,0.25,1],
                    "unit":"color"
                }],
                "wgslBody":"fn effect(uv: vec2f, src: vec4f) -> vec4f { return src; }"
            }"#,
        )
        .expect("valid wire definition");
        assert_eq!(definition.id, "example.tint");
        assert_eq!(definition.inputs, []);
        assert_eq!(definition.params.len(), 1);
        assert_eq!(definition.tier_min, CapabilityTier::TierB);
    }

    #[test]
    fn parses_named_inputs_and_refuses_an_input_count() {
        let definition = EffectRequest::parse(
            r#"{
                "id":"example.displace","version":1,"kind":"effect",
                "inputs":[{"name":"map"}],
                "wgslBody":"fn effect(uv: vec2f, src: vec4f) -> vec4f { return sample_map(uv); }"
            }"#,
        )
        .expect("valid wire definition");
        assert_eq!(definition.inputs, [InputSpec { name: "map".into() }]);
        let error = EffectRequest::parse(
            r#"{"id":"old","version":1,"kind":"effect","inputs":1,"wgslBody":""}"#,
        )
        .expect_err("a count is not a list of inputs");
        assert!(error.contains("invalid type"), "{error}");
        let error = EffectRequest::parse(
            r#"{"id":"odd","version":1,"kind":"effect","inputs":[{"name":"map","wrap":"repeat"}],"wgslBody":""}"#,
        )
        .expect_err("an input has only a name");
        assert!(error.contains("unknown field"), "{error}");
    }

    #[test]
    fn rejects_a_default_with_the_wrong_shape() {
        let error = EffectRequest::parse(
            r#"{
                "id":"bad","version":1,"kind":"effect",
                "params":[{"name":"offset","type":"vec2","default":[1],"unit":"normalized"}],
                "wgslBody":""
            }"#,
        )
        .expect_err("wrong vector shape is rejected");
        assert!(error.contains("offset"));
    }
}
