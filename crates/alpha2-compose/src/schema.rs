//! The published JSON Schemas of the compose spec.
//!
//! They describe shape only: types, required fields, tags and unknown-field rejection. Ranges
//! and file facts are checked by [`crate::compile`].

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// Where the compose schema is published.
pub const COMPOSE_SCHEMA_ID: &str = "https://alpha2.sh/schema/compose.v1.json";

/// Where the core compose schema, the one without the engine-level nodes, is published.
pub const COMPOSE_CORE_SCHEMA_ID: &str = "https://alpha2.sh/schema/compose-core.v1.json";

/// Node types the core schema leaves out: they take engine documents as they are.
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
    value
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
    let defs = schema["$defs"].as_object_mut().expect("the schema has definitions");
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
            let tag = variant["properties"]["type"]["const"].as_str().unwrap_or_default();
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
            object.values_mut().for_each(|value| rewrite_refs(value, shared));
        }
        Value::Array(items) => items.iter_mut().for_each(|value| rewrite_refs(value, shared)),
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
