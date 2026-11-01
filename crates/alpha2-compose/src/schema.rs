//! The published JSON Schema of the compose spec.
//!
//! It describes shape only: types, required fields, tags and unknown-field rejection. Ranges
//! and file facts are checked by [`crate::compile`].

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
