use serde_json::{Value, json};

use crate::builtins::{builtin_effect_groups, builtin_transition_groups};
use crate::{
    EffectDef, EffectKind, EffectResourceKind, ParamSpec, ParamType, ParamUnit, ParamValue,
};

/// Returns the complete engine-owned effect catalog as a host-friendly JSON value.
///
/// Shader source is deliberately excluded: consumers receive the authoring
/// contract while registration and ownership remain inside the engine.
#[must_use]
pub fn builtin_catalog() -> Value {
    let entries = |groups: Vec<(&'static str, Vec<EffectDef>)>| {
        groups
            .into_iter()
            .flat_map(|(group, definitions)| {
                definitions
                    .into_iter()
                    .map(move |definition| catalog_entry(&definition, group))
            })
            .collect::<Vec<_>>()
    };
    json!({
        "effects": entries(builtin_effect_groups()),
        "transitions": entries(builtin_transition_groups()),
    })
}

/// Serializes [`builtin_catalog`] for process and WebAssembly boundaries.
#[must_use]
pub fn builtin_catalog_json() -> String {
    builtin_catalog().to_string()
}

fn catalog_entry(definition: &EffectDef, group: &str) -> Value {
    let label = label(&definition.id);
    json!({
        "id": definition.id,
        "version": definition.version,
        "kind": match definition.kind {
            EffectKind::Effect => "effect",
            EffectKind::Transition => "transition",
            EffectKind::Generator => "generator",
        },
        "inputs": definition
            .inputs
            .iter()
            .map(|input| json!({ "name": input.name }))
            .collect::<Vec<_>>(),
        "label": label,
        "group": group,
        "params": definition.params.iter().map(param).collect::<Vec<_>>(),
        "resources": definition.resources.iter().map(|resource| json!({
            "name": resource.name,
            "kind": match resource.kind { EffectResourceKind::Lut3d => "lut3d" },
        })).collect::<Vec<_>>(),
    })
}

fn param(spec: &ParamSpec) -> Value {
    let value = match &spec.default {
        ParamValue::Float(value) => json!(value),
        ParamValue::Vec2(value) => json!(value),
        ParamValue::Vec4(value) | ParamValue::Color(value) => json!(value),
        ParamValue::Enum(value) => json!(value),
        ParamValue::Bool(value) => json!(value),
    };
    json!({
        "name": spec.name,
        "label": humanize(&spec.name),
        "type": match spec.param_type {
            ParamType::Float => "float",
            ParamType::Vec2 => "vec2",
            ParamType::Vec4 => "vec4",
            ParamType::Color => "color",
            ParamType::Enum => "enum",
            ParamType::Bool => "bool",
        },
        "default": value,
        "min": spec.min,
        "max": spec.max,
        "step": spec.step,
        "unit": match spec.unit {
            ParamUnit::Normalized => "normalized",
            ParamUnit::Degrees => "degrees",
            ParamUnit::Color => "color",
            ParamUnit::Enum => "enum",
            ParamUnit::Bool => "bool",
        },
        "enumValues": spec.enum_values,
        // Whether a project may keyframe it with a curve. Enum and bool values have nothing
        // between them, so their keyframes only hold.
        "animatable": matches!(
            spec.param_type,
            ParamType::Float | ParamType::Vec2 | ParamType::Vec4 | ParamType::Color
        ),
    })
}

/// The labels `humanize` would get wrong: initialisms and the like, by the
/// short id of the built-in.
const LABELS: &[(&str, &str)] = &[
    ("lut-3d", "3D LUT"),
    ("black-and-white", "Black & White"),
    ("hue-saturation", "Hue/Saturation"),
    ("rgb-split", "RGB Split"),
    ("crt", "CRT"),
    ("vhs", "VHS"),
    ("hsv-fade", "HSV Fade"),
    ("tv-static", "TV Static"),
    ("old-tv-lost-signal", "Old TV Lost Signal"),
];

fn label(id: &str) -> String {
    let short = id.rsplit('.').next().unwrap_or(id);
    LABELS
        .iter()
        .find(|(name, _)| *name == short)
        .map_or_else(|| humanize(short), |(_, label)| (*label).to_owned())
}

fn humanize(value: &str) -> String {
    value
        .split(['-', '_'])
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().collect::<String>() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_exposes_lut_resource_without_shader_source() {
        let catalog = builtin_catalog();
        let lut = catalog["effects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == "alpha2.effect.lut-3d")
            .unwrap();
        assert_eq!(lut["resources"][0]["kind"], "lut3d");
        assert_eq!(lut["label"], "3D LUT");
        assert!(lut.get("wgslBody").is_none());

        let crop = catalog["effects"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == "alpha2.effect.crop")
            .unwrap();
        assert_eq!(crop["params"][0]["min"], 0.0);
        assert_eq!(crop["params"][0]["max"], 1.0);
        assert!((crop["params"][0]["step"].as_f64().unwrap() - 0.005).abs() < 1e-8);
    }

    #[test]
    fn every_built_in_lists_the_named_inputs_it_declares() {
        let catalog = builtin_catalog();
        let definitions = crate::builtin_effect_definitions()
            .into_iter()
            .chain(crate::builtin_transition_definitions())
            .collect::<Vec<_>>();
        let entries = catalog["effects"]
            .as_array()
            .unwrap()
            .iter()
            .chain(catalog["transitions"].as_array().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), definitions.len());
        for (entry, definition) in entries.into_iter().zip(&definitions) {
            assert_eq!(entry["id"], definition.id);
            let names = definition
                .inputs
                .iter()
                .map(|input| json!({ "name": input.name }))
                .collect::<Vec<_>>();
            assert_eq!(entry["inputs"], json!(names), "{}", entry["id"]);
        }
    }

    #[test]
    fn every_built_in_is_alpha2_s_own_or_a_gl_transitions_port() {
        // A source with other terms needs a notice and an exception here.
        let own = crate::builtins::ALPHA2_EFFECT_LICENSE;
        for definition in crate::builtin_effect_definitions()
            .into_iter()
            .chain(crate::builtins::internal_effect_definitions())
        {
            assert_eq!(
                definition.license.as_deref(),
                Some(own),
                "{}",
                definition.id
            );
        }
        for definition in crate::builtin_transition_definitions() {
            let license = definition.license.as_deref().unwrap_or_default();
            assert!(
                license == own
                    || (license.starts_with("MIT; Author: ")
                        && license.contains(crate::builtins::GL_TRANSITIONS_BASE)),
                "{}: {license}",
                definition.id
            );
        }
    }

    #[test]
    fn every_label_exception_names_a_built_in_and_changes_its_label() {
        let shorts = crate::builtin_effect_definitions()
            .into_iter()
            .chain(crate::builtin_transition_definitions())
            .map(|definition| definition.id.rsplit('.').next().unwrap().to_owned())
            .collect::<Vec<_>>();
        for (short, label) in LABELS {
            assert!(shorts.iter().any(|name| name == short), "{short}");
            assert_ne!(humanize(short), *label, "{short} needs no exception");
        }
    }

    #[test]
    fn continuous_parameters_are_animatable_and_discrete_ones_hold() {
        let catalog = builtin_catalog();
        let mut seen = std::collections::BTreeSet::new();
        for entry in catalog["effects"]
            .as_array()
            .unwrap()
            .iter()
            .chain(catalog["transitions"].as_array().unwrap())
        {
            for param in entry["params"].as_array().unwrap() {
                let kind = param["type"].as_str().unwrap();
                let animatable = param["animatable"].as_bool().unwrap();
                assert_eq!(
                    animatable,
                    matches!(kind, "float" | "vec2" | "vec4" | "color"),
                    "{}.{}",
                    entry["id"],
                    param["name"]
                );
                seen.insert(kind.to_owned());
            }
        }
        assert!(seen.contains("float") && seen.contains("enum"), "{seen:?}");
    }

    #[test]
    fn a_color_parameter_is_a_color_and_thresholds_are_a_vec4() {
        let catalog = builtin_catalog();
        for entry in catalog["effects"]
            .as_array()
            .unwrap()
            .iter()
            .chain(catalog["transitions"].as_array().unwrap())
        {
            for param in entry["params"].as_array().unwrap() {
                if param["type"] == "color" {
                    assert_eq!(param["unit"], "color", "{}.{}", entry["id"], param["name"]);
                }
            }
        }
        // color-phase's steps are per-channel progress thresholds: keyframed, they move
        // component by component, not as a premultiplied color.
        let phase = catalog["transitions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["id"] == "alpha2.transition.color-phase")
            .unwrap();
        for param in phase["params"].as_array().unwrap() {
            assert_eq!(param["type"], "vec4", "{}", param["name"]);
            assert_eq!(
                (param["min"].as_f64(), param["max"].as_f64()),
                (Some(0.0), Some(1.0))
            );
        }
    }
}
