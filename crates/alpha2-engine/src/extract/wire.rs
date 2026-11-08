//! The JSON forms client-side extractors speak, on every surface that hosts one.

use alpha2_model::{DataInterval, DataWord};
use alpha2_time::MediaTime;
use serde_json::{Value, json};

use super::{Emit, RunContext};

/// An emit as clients write it: `{ output, time, values }` for samples (`null` a lost channel),
/// `{ output, start, end, text, speaker? }` for words, `{ output, start, end, label?, score? }` for
/// intervals.
///
/// # Errors
/// What is missing or mistyped.
pub fn emit_from_json(value: &Value) -> Result<Emit, String> {
    let output = value
        .get("output")
        .and_then(Value::as_str)
        .ok_or("an emit needs `output`")?
        .to_owned();
    let tick = |name: &str| value.get(name).and_then(Value::as_i64).map(MediaTime);
    if let Some(values) = value.get("values") {
        let time = tick("time").ok_or("a sample needs an integer `time`")?;
        let values = values
            .as_array()
            .ok_or("`values` is an array")?
            .iter()
            .map(|value| match value {
                Value::Null => Ok(None),
                #[allow(clippy::cast_possible_truncation)]
                Value::Number(number) => number
                    .as_f64()
                    .map(|number| Some(number as f32))
                    .ok_or("a value is a number or null"),
                _ => Err("a value is a number or null"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        return Ok(Emit::Sample {
            output,
            time,
            values,
        });
    }
    let start = tick("start").ok_or("an item needs an integer `start`")?;
    let end = tick("end").ok_or("an item needs an integer `end`")?;
    let text = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    if let Some(word) = text("text") {
        return Ok(Emit::Word {
            output,
            word: DataWord {
                start,
                end,
                text: word,
                speaker: text("speaker"),
            },
        });
    }
    #[allow(clippy::cast_possible_truncation)]
    let score = value
        .get("score")
        .and_then(Value::as_f64)
        .map(|score| score as f32);
    Ok(Emit::Interval {
        output,
        interval: DataInterval {
            start,
            end,
            label: text("label"),
            score,
        },
    })
}

/// The run context as clients are told it at `begin`.
#[must_use]
pub fn client_context_json(context: &RunContext) -> Value {
    json!({
        "range": context.range,
        "passes": context.passes,
        "from": context.from,
        "video": context.video.map(|(size, format)| json!({ "size": size, "format": format })),
        "audio": context.audio.map(|(rate, channels, block)| json!({ "rate": rate, "channels": channels, "blockFrames": block })),
        "frameRate": context.frame_rate,
    })
}
