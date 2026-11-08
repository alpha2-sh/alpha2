//! The `scripted` test extractor as a plugin. Its params hold the script: `per_delivery`, one list
//! of emits per delivered frame in order, and `at_end`, emitted by `end`. Emits are spelled as in
//! the host protocol's acknowledgements. `fail_at` and `panic_at` make that delivery fail or
//! panic, for loader tests.

use alpha2_plugin_sdk::{Context, Emitter, Error, Extractor, Frame, Host};
use serde_json::Value;

/// The extractor.
pub struct Scripted {
    per_delivery: Vec<Value>,
    at_end: Vec<Value>,
    fail_at: Option<u64>,
    panic_at: Option<u64>,
    delivered: u64,
}

fn list(context: &Context, key: &str) -> Result<Vec<Value>, Error> {
    match context.params.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.clone()),
        Some(_) => Err(format!("params.{key} is not a list").into()),
    }
}

fn emit(out: &mut Emitter, item: &Value) -> Result<(), Error> {
    let text = |key: &str| item.get(key).and_then(Value::as_str);
    let tick = |key: &str| {
        item.get(key)
            .and_then(Value::as_i64)
            .ok_or_else(|| Error::from(format!("an emit has no integer {key}")))
    };
    let output = text("output").ok_or("an emit has no output")?;
    if let Some(values) = item.get("values").and_then(Value::as_array) {
        #[allow(clippy::cast_possible_truncation)]
        let values = values
            .iter()
            .map(|value| value.as_f64().map(|value| value as f32))
            .collect::<Vec<_>>();
        out.sample(output, tick("time")?, &values);
    } else if let Some(word) = text("text") {
        out.word(output, tick("start")?, tick("end")?, word, text("speaker"));
    } else {
        #[allow(clippy::cast_possible_truncation)]
        let score = item
            .get("score")
            .and_then(Value::as_f64)
            .map(|score| score as f32);
        out.interval(output, tick("start")?, tick("end")?, text("label"), score);
    }
    Ok(())
}

impl Extractor for Scripted {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, _host: &Host) -> Result<Self, Error> {
        let index = |key: &str| context.params.get(key).and_then(Value::as_u64);
        Ok(Self {
            per_delivery: list(context, "per_delivery")?,
            at_end: list(context, "at_end")?,
            fail_at: index("fail_at"),
            panic_at: index("panic_at"),
            delivered: 0,
        })
    }

    fn video(&mut self, _frame: &Frame<'_>, out: &mut Emitter) -> Result<(), Error> {
        let delivery = self.delivered;
        self.delivered += 1;
        assert!(
            self.panic_at != Some(delivery),
            "scripted panic at delivery {delivery}"
        );
        if self.fail_at == Some(delivery) {
            return Err(format!("scripted failure at delivery {delivery}").into());
        }
        let items = usize::try_from(delivery)
            .ok()
            .and_then(|delivery| self.per_delivery.get(delivery))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        items.iter().try_for_each(|item| emit(out, item))
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        self.at_end.iter().try_for_each(|item| emit(out, item))
    }
}

alpha2_plugin_sdk::export_extractor!(Scripted);

#[cfg(test)]
mod tests;
