#![no_main]

use alpha2_engine::extract::{ArtifactKind, PluginManifest};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

/// Whether `a` and `b` are the same JSON, a non-integer number allowed one f64 step off: without
/// `float_roundtrip`, serde_json reads some decimals as a neighbor of the nearest double, so the
/// shortest text it writes for a number can read back one step away.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) if x.is_f64() || y.is_f64() => {
            match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) => {
                    x == y || (x.signum() == y.signum() && x.to_bits().abs_diff(y.to_bits()) <= 1)
                }
                _ => false,
            }
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same(x, y))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(key, x)| y.get(key).is_some_and(|y| same(x, y)))
        }
        _ => a == b,
    }
}

// Any text parses as a plugin manifest or is refused; an accepted manifest picks artifacts without
// panicking and survives a JSON round trip, its numbers within a step.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(manifest) = PluginManifest::from_json(text) else {
        return;
    };
    for target in ["wasm32", "x86_64-unknown-linux-gnu", "aarch64-apple-darwin"] {
        for kind in [ArtifactKind::Wasm, ArtifactKind::Cdylib] {
            let _ = manifest.artifact(target, kind);
        }
    }
    let again = serde_json::to_string(&manifest).expect("a manifest serializes");
    let again = PluginManifest::from_json(&again).expect("an accepted manifest reloads");
    let value = |manifest: &PluginManifest| serde_json::to_value(manifest).expect("JSON");
    assert!(
        same(&value(&again), &value(&manifest)),
        "{again:?}\n{manifest:?}"
    );
});
