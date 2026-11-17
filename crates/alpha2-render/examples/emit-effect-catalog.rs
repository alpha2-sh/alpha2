//! Writes the built-in effect catalog to `<schema dir>/effects.v1.json` and to the copy
//! `alpha2-model` embeds.
//!
//! `cargo run -p alpha2-render --example emit-effect-catalog -- <schema dir> <model copy>`

use std::path::PathBuf;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    let dir = args
        .next()
        .map_or_else(|| PathBuf::from("schema"), PathBuf::from);
    let copy = args.next().map_or_else(
        || PathBuf::from("crates/alpha2-model/src/builtin-effects.json"),
        PathBuf::from,
    );
    std::fs::create_dir_all(&dir)?;
    let text = alpha2_render::builtin_catalog_document();
    std::fs::write(dir.join("effects.v1.json"), &text)?;
    std::fs::write(copy, text)
}
