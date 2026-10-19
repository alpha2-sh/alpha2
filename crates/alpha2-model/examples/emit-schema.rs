//! Writes the published project schema and limits.
//!
//! `cargo run -p alpha2-model --features schema --example emit-schema -- <dir>`
//! writes `<dir>/project.v1.schema.json` and `<dir>/project-limits.v1.json`.
//! `scripts/check-schema-fresh.sh` regenerates both and compares them with the
//! committed copies under `schema/`.

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("schema"), PathBuf::from);
    std::fs::create_dir_all(&dir)?;
    for (name, value) in [
        (
            "project.v1.schema.json",
            alpha2_model::project_json_schema(),
        ),
        (
            "project-limits.v1.json",
            alpha2_model::project_limits_json(),
        ),
    ] {
        let mut text = serde_json::to_string_pretty(&value)?;
        text.push('\n');
        std::fs::write(dir.join(name), text)?;
    }
    Ok(())
}
