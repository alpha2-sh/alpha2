//! Writes the published compose spec schema.
//!
//! `cargo run -p alpha2-compose --features schema --example emit-schema -- <dir>`
//! writes `<dir>/compose.v1.schema.json`. `scripts/check-schema-fresh.sh`
//! regenerates it and compares it with the committed copy under `schema/`.

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("schema"), PathBuf::from);
    std::fs::create_dir_all(&dir)?;
    let mut text = serde_json::to_string_pretty(&alpha2_compose::compose_json_schema())?;
    text.push('\n');
    std::fs::write(dir.join("compose.v1.schema.json"), text)
}
