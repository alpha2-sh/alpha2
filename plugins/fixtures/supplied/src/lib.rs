//! A test plugin with a required file slot, `model`, and an optional directory slot, `extra`. At
//! `end` it emits one word per name in `read`, `<name> <length> <first bytes in hex>` or
//! `<name> none`, then one per name in `paths`, `path <name> <path>` or `path <name> none`, and,
//! with `context`, `context <the context's supplied as JSON>`.

use alpha2_plugin_sdk::{Context, Emitter, Error, Extractor, Host};
use serde_json::Value;

/// The extractor.
pub struct Supplied {
    words: Vec<String>,
}

/// The first bytes of a read file the words show.
const SHOWN_BYTES: usize = 8;

fn names(context: &Context, key: &str) -> Result<Vec<String>, Error> {
    match context.params.get(key) {
        None => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| Error::from(format!("params.{key} holds a non-string")))
            })
            .collect(),
        Some(_) => Err(format!("params.{key} is not a list").into()),
    }
}

impl Extractor for Supplied {
    const DESCRIPTOR: &'static str = include_str!("../descriptor.json");

    fn begin(context: &Context, host: &Host) -> Result<Self, Error> {
        let mut words = Vec::new();
        for name in names(context, "read")? {
            words.push(match host.resource(&name) {
                Some(bytes) => {
                    let shown =
                        bytes
                            .iter()
                            .take(SHOWN_BYTES)
                            .fold(String::new(), |mut shown, byte| {
                                use std::fmt::Write;
                                let _ = write!(shown, "{byte:02x}");
                                shown
                            });
                    format!("{name} {} {shown}", bytes.len())
                }
                None => format!("{name} none"),
            });
        }
        for name in names(context, "paths")? {
            words.push(match host.resource_path(&name) {
                Some(path) => format!("path {name} {}", path.display()),
                None => format!("path {name} none"),
            });
        }
        if context.params.get("context") == Some(&Value::Bool(true)) {
            let slots = context
                .supplied
                .iter()
                .map(|(slot, filled)| {
                    let files = filled
                        .files
                        .iter()
                        .map(|file| serde_json::json!({ "name": file.name, "bytes": file.bytes }))
                        .collect::<Vec<_>>();
                    (slot.clone(), serde_json::json!({ "files": files }))
                })
                .collect::<serde_json::Map<_, _>>();
            words.push(format!("context {}", Value::Object(slots)));
        }
        Ok(Self { words })
    }

    fn end(&mut self, out: &mut Emitter) -> Result<(), Error> {
        for (at, word) in (0_i64..).zip(&self.words) {
            out.word("files", at, at + 1, word, None);
        }
        Ok(())
    }
}

alpha2_plugin_sdk::export_extractor!(Supplied);
