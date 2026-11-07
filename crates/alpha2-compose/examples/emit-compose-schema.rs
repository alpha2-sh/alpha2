//! Writes `<dir>/compose.v1.schema.json` and `<dir>/compose-core.v1.schema.json`.
//!
//! `cargo run -p alpha2-compose --features schema --example emit-compose-schema -- <dir>`

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("schema"), PathBuf::from);
    std::fs::create_dir_all(&dir)?;
    for (name, schema) in [
        ("compose.v1.schema.json", alpha2_compose::compose_json_schema()),
        (
            "compose-core.v1.schema.json",
            alpha2_compose::compose_core_json_schema(),
        ),
    ] {
        let mut text = serde_json::to_string_pretty(&schema)?;
        text.push('\n');
        std::fs::write(dir.join(name), text)?;
    }
    Ok(())
}
