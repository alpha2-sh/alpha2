use serde_json::{Value, json};

use crate::{
    EffectDef, EffectKind, EffectResourceKind, ParamSpec, ParamType, ParamUnit, ParamValue,
    builtin_effect_definitions, builtin_transition_definitions,
};

/// Returns the complete engine-owned effect catalog as a host-friendly JSON value.
///
/// Shader source is deliberately excluded: consumers receive the authoring
/// contract while registration and ownership remain inside the engine.
#[must_use]
pub fn builtin_catalog() -> Value {
    let effects = builtin_effect_definitions()
        .iter()
        .map(catalog_entry)
        .collect::<Vec<_>>();
    let transitions = builtin_transition_definitions()
        .iter()
        .map(catalog_entry)
        .collect::<Vec<_>>();
    json!({ "effects": effects, "transitions": transitions })
}

/// Serializes [`builtin_catalog`] for process and WebAssembly boundaries.
#[must_use]
pub fn builtin_catalog_json() -> String {
    builtin_catalog().to_string()
}

fn catalog_entry(definition: &EffectDef) -> Value {
    let (label, group) = presentation(&definition.id, definition.kind);
    json!({
        "id": definition.id,
        "version": definition.version,
        "kind": match definition.kind {
            EffectKind::Effect => "effect",
            EffectKind::Transition => "transition",
            EffectKind::Generator => "generator",
        },
        "inputs": match definition.inputs {
            crate::EffectInputs::None => 0,
            crate::EffectInputs::One => 1,
            crate::EffectInputs::Two => 2,
        },
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

fn presentation(id: &str, kind: EffectKind) -> (String, &'static str) {
    let short = id.rsplit('.').next().unwrap_or(id);
    let label = if short == "lut-3d" {
        "3D LUT".into()
    } else {
        humanize(short)
    };
    let group = match kind {
        EffectKind::Effect if matches!(short, "gaussian-blur" | "sharpen") => "Blur & Sharpen",
        EffectKind::Effect if short == "chroma-key" => "Keying",
        EffectKind::Effect if matches!(short, "color-controls" | "lut-3d") => "Color",
        EffectKind::Effect if matches!(short, "crop" | "mirror" | "pixelate") => "Distort",
        EffectKind::Effect => "Stylize",
        EffectKind::Transition
            if matches!(
                short,
                "wipe-up"
                    | "wipe-down"
                    | "wipe-left"
                    | "wipe-right"
                    | "window-blinds"
                    | "window-slice"
                    | "wind"
                    | "pinwheel"
                    | "heart"
            ) =>
        {
            "Wipes"
        }
        EffectKind::Transition
            if matches!(
                short,
                "fade"
                    | "directional"
                    | "circle-open"
                    | "angular"
                    | "pixelize"
                    | "directional-wipe"
                    | "circle"
                    | "cross-warp"
                    | "dreamy"
                    | "radial"
            ) =>
        {
            "Classic"
        }
        EffectKind::Transition => "Stylized",
        EffectKind::Generator => "Generate",
    };
    (label, group)
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
