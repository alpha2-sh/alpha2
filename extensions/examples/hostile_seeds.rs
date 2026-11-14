//! Writes the hostile test documents of one fuzz target into a directory, as seeds.
//!
//! Usage: `cargo run -p alpha2-extensions --example hostile_seeds -- <target> <dir>`

#[cfg(any(feature = "svg", feature = "exr"))]
#[path = "../src/tests/hostile.rs"]
mod hostile;

type Seeds = Vec<(&'static str, Vec<u8>)>;

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let (Some(target), Some(dir)) = (args.next(), args.next()) else {
        eprintln!("usage: hostile_seeds <target> <dir>");
        std::process::exit(2);
    };
    let seeds = match target.as_str() {
        "extension_svg" => svg_seeds(),
        "extension_exr" => exr_seeds(),
        _ => Vec::new(),
    };
    if seeds.is_empty() {
        eprintln!("no hostile seeds for {target} with these features");
    }
    std::fs::create_dir_all(&dir)?;
    for (name, bytes) in seeds {
        std::fs::write(std::path::Path::new(&dir).join(name), bytes)?;
    }
    Ok(())
}

#[cfg(feature = "svg")]
fn svg_seeds() -> Seeds {
    use hostile::{svg_entity_elements, svg_nested_entity_elements, svg_nested_images};
    vec![
        (
            "entity-elements.svg",
            svg_entity_elements(1000, 10_000).into(),
        ),
        (
            "nested-entity-elements.svg",
            svg_nested_entity_elements(100, 255, 400).into(),
        ),
        (
            "nested-images.svg",
            svg_nested_images(&svg_entity_elements(1000, 10_000), 2).into(),
        ),
    ]
}

#[cfg(not(feature = "svg"))]
fn svg_seeds() -> Seeds {
    Vec::new()
}

#[cfg(feature = "exr")]
fn exr_seeds() -> Seeds {
    use hostile::{ExrChunks, ExrPart, exr_file};
    let tile = |extra, chunks| ExrPart {
        width: 3840,
        height: 2160,
        extra,
        tiled: true,
        chunks,
    };
    let lines = ExrPart {
        width: 64,
        height: 32,
        extra: 40,
        tiled: false,
        chunks: ExrChunks::Zeros,
    };
    vec![
        ("tile-400.exr", exr_file(&[tile(400, ExrChunks::Empty)])),
        ("tile-4000.exr", exr_file(&[tile(4000, ExrChunks::Empty)])),
        (
            "tile-400-claim.exr",
            exr_file(&[tile(400, ExrChunks::Claim(2_000_000_000))]),
        ),
        ("lines-40.exr", exr_file(&[lines])),
        ("parts.exr", exr_file(&[lines, tile(400, ExrChunks::Empty)])),
    ]
}

#[cfg(not(feature = "exr"))]
fn exr_seeds() -> Seeds {
    Vec::new()
}
