//! The published JSON Schema of the compose spec.
//!
//! As the project schema, it describes shape: types, required fields, tags,
//! and unknown-field rejection. Ranges, the facts a spec needs, and whether
//! it compiles are [`crate::compile`]'s to decide. The model types a spec
//! embeds (a layout's root, a raw fragment's tracks) are stated as the
//! project schema states them.

use serde_json::Value;

/// The JSON Schema (draft 2020-12) of the current compose spec.
#[must_use]
pub fn compose_json_schema() -> Value {
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
