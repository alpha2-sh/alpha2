//! The published JSON Schema of the project document.
//!
//! The schema describes shape: JSON types, required and optional fields,
//! enumerated and tagged values, and unknown-field rejection. What a document
//! means (value ranges, limits, references between items, keyframe order,
//! transition coverage) is decided by [`crate::Project::validate`] and pinned
//! by the shared validation corpus. A document the schema rejects is never a
//! valid project; one it accepts may still be.
//!
//! The parser also refuses what a schema cannot say: a frame rate faster
//! than one frame per tick (a rule between two fields), an integer written
//! with a fraction or exponent, a struct field given twice, and a string that
//! is not well-formed Unicode. `validateProject` and `validateProjectJson`
//! check those as shape problems; the corpus calls them `parser` cases.

use crate::project::ProjectWire;
use serde_json::{Value, json};

/// The JSON Schema (draft 2020-12) of the current project document.
#[must_use]
pub fn project_json_schema() -> Value {
    let mut schema = schemars::generate::SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator()
        .into_root_schema_for::<ProjectWire>();
    schema.insert(
        "title".to_owned(),
        Value::String(format!(
            "alpha2 project document, schema version {}",
            crate::CURRENT_SCHEMA_VERSION
        )),
    );
    let mut value = schema.to_value();
    bound_unsigned_integers(&mut value);
    shortest_defaults(&mut value);
    value
}

/// Writes each `default` the way the engine writes the document
/// ([`shortest_floats`]): schemars widens a 32-bit default to 64 bits.
fn shortest_defaults(node: &mut Value) {
    match node {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == "default" {
                    shortest_floats(value);
                } else {
                    shortest_defaults(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(shortest_defaults),
        _ => {}
    }
}

/// Gives every `uint32` the upper bound the parser enforces. schemars states
/// the type in `format`, which validators do not check, and the lower bound
/// alone.
fn bound_unsigned_integers(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if map.get("format").and_then(Value::as_str) == Some("uint32") {
                map.entry("maximum").or_insert_with(|| u32::MAX.into());
            }
            map.values_mut().for_each(bound_unsigned_integers);
        }
        Value::Array(items) => items.iter_mut().for_each(bound_unsigned_integers),
        _ => {}
    }
}

/// The portable document limits, as the JSON object the TypeScript constants
/// are generated from. A 32-bit float limit is written as its shortest
/// decimal, as the engine writes such a number ([`shortest_floats`]).
#[must_use]
// One table row per limit: its length is the number of limits.
#[allow(clippy::too_many_lines)]
pub fn project_limits_json() -> Value {
    let limits = [
        (
            "CURRENT_SCHEMA_VERSION",
            json!(crate::CURRENT_SCHEMA_VERSION),
        ),
        (
            "MAX_PROJECT_JSON_BYTES",
            json!(crate::MAX_PROJECT_JSON_BYTES),
        ),
        ("MAX_CANVAS_DIMENSION", json!(crate::MAX_CANVAS_DIMENSION)),
        ("MAX_CANVAS_PIXELS", json!(crate::MAX_CANVAS_PIXELS)),
        ("MIN_SAMPLE_RATE", json!(crate::SAMPLE_RATE_RANGE.start())),
        ("MAX_SAMPLE_RATE", json!(crate::SAMPLE_RATE_RANGE.end())),
        (
            "MIN_HDR_PEAK_NITS",
            json!(crate::HDR_PEAK_NITS_RANGE.start()),
        ),
        ("MAX_HDR_PEAK_NITS", json!(crate::HDR_PEAK_NITS_RANGE.end())),
        ("MAX_TRACKS", json!(crate::MAX_TRACKS)),
        ("MAX_CLIPS", json!(crate::MAX_CLIPS)),
        ("MAX_CLIPS_PER_TRACK", json!(crate::MAX_CLIPS_PER_TRACK)),
        ("MAX_COMPOSITIONS", json!(crate::MAX_COMPOSITIONS)),
        ("MAX_COMPOSITION_NODES", json!(crate::MAX_COMPOSITION_NODES)),
        ("MAX_GROUP_INPUTS", json!(crate::MAX_GROUP_INPUTS)),
        (
            "MAX_COMPOSITION_CONTROLS",
            json!(crate::MAX_COMPOSITION_CONTROLS),
        ),
        ("MAX_COMPOSITION_DEPTH", json!(crate::MAX_COMPOSITION_DEPTH)),
        ("MAX_INLINED_NODES", json!(crate::MAX_INLINED_NODES)),
        (
            "MAX_FRAME_INLINED_NODES",
            json!(crate::MAX_FRAME_INLINED_NODES),
        ),
        ("MAX_MERGE_LAYERS", json!(crate::MAX_MERGE_LAYERS)),
        ("MAX_CLIP_MASKS", json!(crate::MAX_CLIP_MASKS)),
        ("MAX_GRADIENT_STOPS", json!(crate::MAX_GRADIENT_STOPS)),
        (
            "MAX_TEXT_CONTENT_BYTES",
            json!(crate::MAX_TEXT_CONTENT_BYTES),
        ),
        (
            "MAX_CAPTION_FILE_BYTES",
            json!(crate::MAX_CAPTION_FILE_BYTES),
        ),
        ("MAX_CAPTION_FILE_CUES", json!(crate::MAX_CAPTION_FILE_CUES)),
        ("MAX_FONT_FAMILY_BYTES", json!(crate::MAX_FONT_FAMILY_BYTES)),
        (
            "MAX_TRANSITIONS_PER_TRACK",
            json!(crate::MAX_TRANSITIONS_PER_TRACK),
        ),
        ("MAX_GAIN", json!(crate::MAX_GAIN)),
        ("MAX_AUDIO_EFFECTS", json!(crate::MAX_AUDIO_EFFECTS)),
        ("MAX_AUDIO_DELAY_MS", json!(crate::MAX_AUDIO_DELAY_MS)),
        ("REVERB_DELAY_MS", json!(crate::REVERB_DELAY_MS)),
        (
            "AUDIO_EFFECT_STATE_MARGIN_MS",
            json!(crate::AUDIO_EFFECT_STATE_MARGIN_MS),
        ),
        ("MAX_EQ_GAIN_DB", json!(crate::MAX_EQ_GAIN_DB)),
        ("MIN_EQ_Q", json!(crate::EQ_Q_RANGE.start())),
        ("MAX_EQ_Q", json!(crate::EQ_Q_RANGE.end())),
        (
            "MIN_EQ_FREQUENCY_HZ",
            json!(crate::EQ_FREQUENCY_RANGE_HZ.start()),
        ),
        (
            "MAX_EQ_FREQUENCY_HZ",
            json!(crate::EQ_FREQUENCY_RANGE_HZ.end()),
        ),
        ("MAX_COMPRESSOR_RATIO", json!(crate::MAX_COMPRESSOR_RATIO)),
        ("MAX_CHAIN_EFFECTS", json!(crate::MAX_CHAIN_EFFECTS)),
        ("MAX_EQ_BANDS", json!(crate::MAX_EQ_BANDS)),
        ("MIN_LEVEL_DB", json!(crate::MIN_LEVEL_DB)),
        ("MAX_LOOKAHEAD_MS", json!(crate::MAX_LOOKAHEAD_MS)),
        ("MAX_ENVELOPE_MS", json!(crate::MAX_ENVELOPE_MS)),
        ("MAX_CAPTION_FONT_SIZE", json!(crate::MAX_CAPTION_FONT_SIZE)),
        (
            "MAX_TEXT_GENERATOR_FONT_SIZE",
            json!(crate::MAX_TEXT_GENERATOR_FONT_SIZE),
        ),
        ("MIN_LINE_HEIGHT", json!(crate::LINE_HEIGHT_RANGE.start())),
        ("MAX_LINE_HEIGHT", json!(crate::LINE_HEIGHT_RANGE.end())),
        (
            "MAX_TEXT_OUTLINE_WIDTH",
            json!(crate::MAX_TEXT_OUTLINE_WIDTH),
        ),
        ("MAX_TEXT_PADDING", json!(crate::MAX_TEXT_PADDING)),
        ("MAX_SEQUENCE_FRAMES", json!(crate::MAX_SEQUENCE_FRAMES)),
        (
            "MAX_PROVIDER_DOCUMENT_BYTES",
            json!(crate::MAX_PROVIDER_DOCUMENT_BYTES),
        ),
        (
            "MAX_LOTTIE_DOCUMENT_BYTES",
            json!(crate::MAX_LOTTIE_DOCUMENT_BYTES),
        ),
        (
            "MAX_SVG_DOCUMENT_BYTES",
            json!(crate::MAX_SVG_DOCUMENT_BYTES),
        ),
        ("TICKS_PER_SECOND", json!(alpha2_time::TICKS_PER_SECOND)),
    ];
    let mut value = Value::Object(
        limits
            .into_iter()
            .map(|(name, limit)| (name.to_owned(), limit))
            .collect(),
    );
    shortest_floats(&mut value);
    value
}

/// Writes every number that is exactly a 32-bit float as that float's
/// shortest decimal: `0.05`, not the `0.05000000074505806` its widening to
/// 64 bits spells. The model's fractional values are 32-bit, and the engine
/// writes them shortest, so a default or limit reads as the document does.
/// A 64-bit value is never exactly a 32-bit float unless it is one, like
/// `0.5`, whose spelling this does not change.
// Narrowing to 32 bits is the test: a value it changes is not a 32-bit float.
#[allow(clippy::cast_possible_truncation)]
fn shortest_floats(node: &mut Value) {
    match node {
        Value::Number(number) if number.is_f64() => {
            if let Some(wide) = number.as_f64()
                // Exactly a 32-bit float: the cast is the value itself.
                && f64::from(wide as f32).to_bits() == wide.to_bits()
                && let Ok(short) = (wide as f32).to_string().parse::<f64>()
                && let Some(short) = serde_json::Number::from_f64(short)
            {
                *number = short;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(shortest_floats),
        Value::Object(map) => map.values_mut().for_each(shortest_floats),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// Every float in `node` that spells more digits than the 32-bit value it
    /// is needs: one the engine would never write.
    #[allow(clippy::cast_possible_truncation)]
    fn widened(node: &Value, path: &str, found: &mut Vec<String>) {
        match node {
            Value::Number(number) => {
                if let Some(wide) = number.as_f64().filter(|_| number.is_f64())
                    && f64::from(wide as f32).to_bits() == wide.to_bits()
                    && number.to_string().len() > (wide as f32).to_string().len() + 2
                {
                    found.push(format!("{path} = {number}"));
                }
            }
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    widened(item, &format!("{path}[{index}]"), found);
                }
            }
            Value::Object(map) => {
                for (key, value) in map {
                    widened(value, &format!("{path}.{key}"), found);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn floats_are_published_as_the_engine_writes_them() {
        let mut found = Vec::new();
        widened(&super::project_json_schema(), "schema", &mut found);
        widened(&super::project_limits_json(), "limits", &mut found);
        assert!(found.is_empty(), "{found:#?}");
        let limits = super::project_limits_json();
        assert_eq!(limits["MIN_EQ_Q"].to_string(), "0.05");
        let schema = super::project_json_schema().to_string();
        assert!(schema.contains("\"default\":0.05"), "caption font size");
    }
}
