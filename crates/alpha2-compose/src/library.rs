//! The 1st-party templates every compose build holds, under `alpha2/`.

use std::sync::LazyLock;

use serde_json::Value;

/// Their sources, one JSON document each.
const SOURCES: [&str; 2] = [
    include_str!("templates/circle-reveal.json"),
    include_str!("templates/wipe-reveal.json"),
];

/// The 1st-party templates, parsed.
pub(crate) static TEMPLATES: LazyLock<Vec<Value>> = LazyLock::new(|| {
    SOURCES
        .iter()
        .map(|source| serde_json::from_str(source).expect("a built-in template is JSON"))
        .collect()
});
