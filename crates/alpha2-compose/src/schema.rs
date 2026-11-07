//! The published JSON Schemas of the compose spec.
//!
//! They describe shape only: types, required fields, tags and unknown-field rejection. Ranges
//! and file facts are checked by [`crate::compile`].

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

pub use crate::spec::COMPOSE_SCHEMA_ID;

/// Where the core compose schema, the one without the engine-level nodes, is published.
pub const COMPOSE_CORE_SCHEMA_ID: &str = "https://alpha2.sh/schema/compose-core.v1.json";

/// Node types the core schema leaves out, since they take engine documents as they are.
const ADVANCED_NODES: [&str; 2] = ["raw", "layout"];

/// The compose spec as schemars writes it, every type inline.
fn generated() -> Value {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<crate::VideoSpec>();
    schema.insert(
        "title".to_owned(),
        Value::String(format!(
            "alpha2 compose spec, version {}",
            crate::COMPOSE_VERSION
        )),
    );
    let mut value = schema.to_value();
    alpha2_model::finish_json_schema(&mut value);
    value["$defs"]["EffectUse"] = effect_use_schema();
    if let Some(nodes) = value["$defs"]["Node"]["oneOf"].as_array_mut() {
        nodes.extend(crate::template::node_schemas());
    }
    if let Some(defs) = value["$defs"].as_object_mut() {
        defs.extend(animation_defs());
    }
    value
}

/// An effect use as one variant per built-in effect, its parameters typed and ranged as the
/// catalog declares them, plus any effect by its full id.
fn effect_use_schema() -> Value {
    let mut variants = alpha2_model::builtin_effects()
        .iter()
        .filter(|spec| spec.kind == alpha2_model::EffectKind::Effect)
        .map(|spec| {
            let short = spec
                .id
                .strip_prefix("alpha2.effect.")
                .unwrap_or(&spec.id)
                .to_owned();
            let mut properties = Map::new();
            properties.insert("effect".to_owned(), json!({"const": short}));
            properties.insert(
                "version".to_owned(),
                json!({"const": spec.version, "description": "The built-in's version."}),
            );
            for param in &spec.params {
                properties.insert(
                    param.name.clone(),
                    animated(&param_value_schema(param), base_name(param.param_type)),
                );
            }
            for input in &spec.inputs {
                properties.insert(
                    input.clone(),
                    json!({"$ref": "#/$defs/Node", "description": "A node drawn as this image."}),
                );
            }
            let mut required = vec![json!("effect")];
            required.extend(spec.inputs.iter().map(|input| json!(input)));
            json!({
                "type": "object",
                "title": spec.label,
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            })
        })
        .collect::<Vec<_>>();
    variants.push(json!({
        "type": "object",
        "description": "A host's effect by its full id, with its parameters by name.",
        "properties": {
            "effect": {"type": "string", "pattern": "^[^.]+\\..+$"},
            "version": {"type": "integer", "minimum": 1},
        },
        "required": ["effect"],
        "additionalProperties": true,
    }));
    json!({
        "description": "One effect: a built-in by its short name, or any effect by its full id, with its parameters by name. A parameter is a constant, a ramp or keys; a named image input is a node.",
        "oneOf": variants,
    })
}

/// The base type a parameter's ramps and keys use.
const fn base_name(param_type: alpha2_model::EffectParamType) -> &'static str {
    use alpha2_model::EffectParamType as Type;
    match param_type {
        Type::Float => "number",
        Type::Vec2 => "vec2",
        Type::Vec4 => "vec4",
        Type::Color => "color",
        Type::Enum => "choice",
        Type::Bool => "bool",
    }
}

/// What one value of a built-in's parameter is written as.
fn param_value_schema(param: &alpha2_model::EffectParamSpec) -> Value {
    use alpha2_model::EffectParamType as Type;
    let number = || {
        let mut number = json!({"type": "number"});
        if let Some(min) = param.min {
            number["minimum"] = json!(min);
        }
        if let Some(max) = param.max {
            number["maximum"] = json!(max);
        }
        number
    };
    let mut schema = match param.param_type {
        Type::Float => number(),
        Type::Vec2 => json!({"type": "array", "items": number(), "minItems": 2, "maxItems": 2}),
        Type::Vec4 => json!({"type": "array", "items": number(), "minItems": 4, "maxItems": 4}),
        Type::Color => json!({"$ref": "#/$defs/Rgba"}),
        Type::Enum => json!({"enum": param.enum_values}),
        Type::Bool => json!({"type": "boolean"}),
    };
    if let Some(object) = schema.as_object_mut() {
        object.insert("title".to_owned(), json!(param.label));
    }
    schema
}

/// A constant `value`, or a ramp or keys of its base type, by reference.
fn animated(value: &Value, base: &str) -> Value {
    json!({"anyOf": [
        value,
        {"$ref": format!("#/$defs/EffectRamp_{base}")},
        {"$ref": format!("#/$defs/EffectKeys_{base}")},
    ]})
}

/// The ramp and keys of each parameter base type, shared by every effect.
fn animation_defs() -> Vec<(String, Value)> {
    let ease = json!({"$ref": "#/$defs/EasingChoice"});
    let bases = [
        ("number", json!({"type": "number"})),
        (
            "vec2",
            json!({"type": "array", "items": {"type": "number"}, "minItems": 2, "maxItems": 2}),
        ),
        (
            "vec4",
            json!({"type": "array", "items": {"type": "number"}, "minItems": 4, "maxItems": 4}),
        ),
        ("color", json!({"$ref": "#/$defs/Rgba"})),
        ("choice", json!({"type": "string"})),
        ("bool", json!({"type": "boolean"})),
    ];
    bases
        .into_iter()
        .flat_map(|(base, value)| {
            [
                (
                    format!("EffectRamp_{base}"),
                    json!({
                        "type": "object",
                        "description": "A change from `from` to `to` over the node's time.",
                        "properties": {
                            "from": value,
                            "to": value,
                            "ease": ease,
                            "at": {"type": "number", "minimum": 0},
                            "duration": {"type": "number", "minimum": 0},
                        },
                        "required": ["from", "to"],
                        "additionalProperties": false,
                    }),
                ),
                (
                    format!("EffectKeys_{base}"),
                    json!({
                        "type": "object",
                        "description": "Values at seconds from the node's start.",
                        "properties": {
                            "keys": {
                                "type": "array",
                                "minItems": 1,
                                "items": {
                                    "type": "object",
                                    "properties": {"t": {"type": "number", "minimum": 0}, "v": value, "ease": ease},
                                    "required": ["t", "v"],
                                    "additionalProperties": false,
                                },
                            },
                        },
                        "required": ["keys"],
                        "additionalProperties": false,
                    }),
                ),
            ]
        })
        .collect()
}

/// The JSON Schema (draft 2020-12) of the current compose spec. Types it shares with the project
/// document are `$ref`s into the project schema rather than copies.
///
/// # Panics
/// Never; a generated root schema has definitions.
#[must_use]
pub fn compose_json_schema() -> Value {
    let mut schema = generated();
    let project = alpha2_model::project_json_schema();
    let project_defs = project["$defs"].as_object().cloned().unwrap_or_default();
    let defs = schema["$defs"]
        .as_object_mut()
        .expect("the schema has definitions");
    let shared = defs
        .iter()
        .filter(|(name, def)| project_defs.get(*name) == Some(*def))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();
    defs.retain(|name, _| !shared.contains(name));
    rewrite_refs(&mut schema, &shared);
    insert_front(&mut schema, "$id", COMPOSE_SCHEMA_ID);
    schema
}

/// The compose spec without its engine-level nodes (`raw`, `layout`), self-contained: the
/// schema an agent writes specs against.
#[must_use]
pub fn compose_core_json_schema() -> Value {
    let mut schema = generated();
    if let Some(node) = schema["$defs"]["Node"]["oneOf"].as_array_mut() {
        node.retain(|variant| {
            let tag = variant["properties"]["type"]["const"]
                .as_str()
                .unwrap_or_default();
            !ADVANCED_NODES.contains(&tag)
        });
    }
    prune(&mut schema);
    insert_front(&mut schema, "$id", COMPOSE_CORE_SCHEMA_ID);
    if let Some(object) = schema.as_object_mut() {
        object.insert(
            "title".to_owned(),
            Value::String(format!(
                "alpha2 compose spec, core, version {}",
                crate::COMPOSE_VERSION
            )),
        );
    }
    schema
}

fn insert_front(schema: &mut Value, key: &str, value: &str) {
    if let Some(object) = schema.as_object_mut() {
        let rest = std::mem::take(object);
        object.insert(key.to_owned(), Value::String(value.to_owned()));
        object.extend(rest);
    }
}

/// Points every local `$ref` to a shared type at the project schema.
fn rewrite_refs(value: &mut Value, shared: &BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(Value::String(reference)) = object.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/$defs/")
                && shared.contains(name)
            {
                *reference = format!("{}#/$defs/{name}", alpha2_model::PROJECT_SCHEMA_ID);
            }
            object
                .values_mut()
                .for_each(|value| rewrite_refs(value, shared));
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|value| rewrite_refs(value, shared)),
        _ => {}
    }
}

/// Drops the definitions nothing reachable from the root names.
fn prune(schema: &mut Value) {
    let defs = schema["$defs"].as_object().cloned().unwrap_or_default();
    let mut reached = BTreeSet::new();
    let mut pending = Vec::new();
    let mut root = schema.clone();
    if let Some(object) = root.as_object_mut() {
        object.remove("$defs");
    }
    refs(&root, &mut pending);
    while let Some(name) = pending.pop() {
        if reached.insert(name.clone())
            && let Some(def) = defs.get(&name)
        {
            refs(def, &mut pending);
        }
    }
    let kept = defs
        .into_iter()
        .filter(|(name, _)| reached.contains(name))
        .collect::<Map<_, _>>();
    schema["$defs"] = Value::Object(kept);
}

fn refs(value: &Value, found: &mut Vec<String>) {
    match value {
        Value::Object(object) => {
            if let Some(name) = object
                .get("$ref")
                .and_then(Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                found.push(name.to_owned());
            }
            object.values().for_each(|value| refs(value, found));
        }
        Value::Array(items) => items.iter().for_each(|value| refs(value, found)),
        _ => {}
    }
}
