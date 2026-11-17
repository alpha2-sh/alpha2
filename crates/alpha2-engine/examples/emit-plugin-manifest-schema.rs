//! Writes `<dir>/plugin-manifest.v1.json`.
//!
//! `cargo run -p alpha2-engine --features schema --example emit-plugin-manifest-schema -- <dir>`

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let dir = std::env::args_os()
        .nth(1)
        .map_or_else(|| PathBuf::from("schema"), PathBuf::from);
    std::fs::create_dir_all(&dir)?;
    let mut text =
        serde_json::to_string_pretty(&alpha2_engine::extract::plugin_manifest_json_schema())?;
    text.push('\n');
    std::fs::write(dir.join("plugin-manifest.v1.json"), text)
}
