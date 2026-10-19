//! Build-time WESL compilation for the complete shader modules.
//!
//! Every entry under `shaders/build` is resolved and flattened into a single
//! WGSL module in `OUT_DIR`; `src/lib.rs` embeds the results with `include_str!`.

use std::fs;
use std::path::PathBuf;

/// WESL entry points, named without their extension.
const ENTRY_POINTS: &[&str] = &[
    "canvas-present",
    "fullscreen-quad",
    "composite",
    "readback-pack",
    "rgba-present",
    "rgb-to-yuv420",
    "yuv-repack",
    "yuv-to-linear",
];

/// Entry points compiled a second time with one WESL feature enabled, as
/// (entry, feature, output name).
const VARIANTS: &[(&str, &str, &str)] = &[("yuv-to-linear", "u16_planes", "yuv16-to-linear")];

fn main() {
    println!("cargo:rerun-if-changed=shaders/build");
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("Cargo OUT_DIR"));
    let builds = ENTRY_POINTS
        .iter()
        .map(|entry| (*entry, None, *entry))
        .chain(
            VARIANTS
                .iter()
                .map(|(entry, feature, name)| (*entry, Some(*feature), *name)),
        );
    for (entry, feature, name) in builds {
        let mut compiler = wesl::Wesl::new("shaders/build");
        for (_, known, _) in VARIANTS {
            compiler.set_feature(known, feature == Some(*known));
        }
        let module = wesl::ModulePath::from_path(format!("{entry}.wesl"));
        let wgsl = compiler
            .compile(module)
            .unwrap_or_else(|error| panic!("compile {entry}.wesl as {name}: {error}"))
            .to_string();
        fs::write(out_dir.join(format!("{name}.wgsl")), wgsl)
            .unwrap_or_else(|error| panic!("write {name}.wgsl: {error}"));
    }
}
