//! The 1st-party templates every compose build holds, under `alpha2/`.

use std::sync::LazyLock;

use serde_json::Value;

/// Their sources, one JSON document each.
const SOURCES: [&str; 9] = [
    include_str!("templates/circle-reveal.json"),
    include_str!("templates/wipe-reveal.json"),
    include_str!("templates/title.json"),
    include_str!("templates/title-card.json"),
    include_str!("templates/lower-third.json"),
    include_str!("templates/end-card.json"),
    include_str!("templates/logo.json"),
    include_str!("templates/reframe.json"),
    include_str!("templates/comparison-grid.json"),
];

/// The 1st-party templates, parsed.
pub(crate) static TEMPLATES: LazyLock<Vec<Value>> = LazyLock::new(|| {
    SOURCES
        .iter()
        .map(|source| serde_json::from_str(source).expect("a built-in template is JSON"))
        .collect()
});
