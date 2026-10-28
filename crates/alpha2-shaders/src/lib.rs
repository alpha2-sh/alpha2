//! Reusable shader assets for the alpha2 render pipeline.
//!
//! Shader source lives under `shaders/`, never inline in Rust, and is split by
//! how it reaches the GPU:
//!
//! - `shaders/build/*.wesl` are complete modules with their own entry points.
//!   `build.rs` resolves their imports and flattens each one into WGSL at build
//!   time, so they are embedded here already compiled.
//! - `shaders/runtime/**/*.wgsl` are effect *bodies*, not modules. They are not
//!   valid on their own: `alpha2-render` generates a per-effect preamble from
//!   the effect's parameter schema and concatenates the body onto it, which is
//!   why these cannot be resolved ahead of time.

mod effects;
mod transitions;

pub use effects::*;
pub use transitions::*;

/// Embeds a WESL module compiled by `build.rs` into `OUT_DIR`.
macro_rules! built_module {
    ($entry:literal) => {
        include_str!(concat!(env!("OUT_DIR"), "/", $entry, ".wgsl"))
    };
}

/// Full-screen triangle drawing a rendered frame onto a browser canvas.
pub const CANVAS_PRESENT_WGSL: &str = built_module!("canvas-present");
/// Full-screen triangle copying a premultiplied-alpha texel.
pub const FULLSCREEN_TEXTURE_QUAD_WGSL: &str = built_module!("fullscreen-quad");
/// Transform and blend one premultiplied RGBA layer over a backdrop.
pub const COMPOSITE_WGSL: &str = built_module!("composite");
/// GPU quantization from float intermediates into an RGBA8 readback texture.
pub const READBACK_PACK_WGSL: &str = built_module!("readback-pack");
/// GPU contain/rotation/premultiplication for browser-imported RGBA frames.
pub const RGBA_PRESENT_WGSL: &str = built_module!("rgba-present");
/// GPU conversion from premultiplied RGB intermediates into planar YUV420.
pub const RGB_TO_YUV420_WGSL: &str = built_module!("rgb-to-yuv420");
/// Sample-for-sample copy of decoded 8-bit 4:2:0 planes into NV12 targets.
pub const YUV_REPACK_WGSL: &str = built_module!("yuv-repack");
/// Limited/full-range BT.601/709/2020-NCL YUV plane conversion into linear RGB.
pub const YUV_TO_LINEAR_WGSL: &str = built_module!("yuv-to-linear");
/// [`YUV_TO_LINEAR_WGSL`] for 16-bit containers uploaded as unsigned
/// integers, which it filters and normalizes itself.
pub const YUV16_TO_LINEAR_WGSL: &str = built_module!("yuv16-to-linear");

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    fn shader_files(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).expect("shader directory") {
            let path = entry.expect("shader entry").path();
            if path.is_dir() {
                shader_files(&path, files);
            } else if path
                .extension()
                .is_some_and(|extension| extension == "wgsl" || extension == "wesl")
            {
                files.push(path);
            }
        }
    }

    /// `text` without `//` comments and with the whitespace gone, which is
    /// all a search for a call needs. The shaders write no `/* */` comments.
    fn code(text: &str) -> String {
        text.lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .flat_map(str::chars)
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    #[test]
    fn no_built_in_shader_hashes_through_sin() {
        // `fract(sin(x) * 43758.5453)` is not a hash on every GPU: large-argument `sin` has no
        // required precision, so each driver's error gives a different value.
        let mut files = Vec::new();
        shader_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders"),
            &mut files,
        );
        assert!(files.len() > 10, "found only {files:?}");
        for file in files {
            let text = std::fs::read_to_string(&file).expect("shader text");
            assert!(!text.contains("/*"), "{}: block comment", file.display());
            let code = code(&text);
            assert!(
                !code.contains("fract(sin("),
                "{} hashes through fract(sin(...)); use an integer hash (PCG) instead",
                file.display()
            );
        }
    }

    /// The first two arguments, without whitespace, of every call to the
    /// built-in `smoothstep` in `text`, not counting the helpers whose names
    /// end in it.
    fn smoothstep_edges(text: &str) -> Vec<(String, String)> {
        let text = text
            .lines()
            .map(|line| line.split("//").next().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        let mut found = Vec::new();
        let mut search = 0;
        while let Some(offset) = text[search..].find("smoothstep") {
            let start = search + offset;
            search = start + "smoothstep".len();
            let before = text[..start].chars().next_back();
            let rest = text[search..].trim_start();
            if before.is_some_and(|character| character.is_ascii_alphanumeric() || character == '_')
                || !rest.starts_with('(')
            {
                continue;
            }
            let (mut depth, mut arguments, mut argument) = (0_usize, Vec::new(), String::new());
            for character in rest[1..].chars() {
                match character {
                    '(' | '[' => depth += 1,
                    ')' | ']' if depth == 0 => break,
                    ')' | ']' => depth -= 1,
                    ',' if depth == 0 => {
                        arguments.push(std::mem::take(&mut argument));
                        continue;
                    }
                    _ if character.is_whitespace() => continue,
                    _ => {}
                }
                argument.push(character);
            }
            // A trailing comma leaves nothing after it.
            if !argument.is_empty() {
                arguments.push(argument);
            }
            let [edge0, edge1, _] = <[String; 3]>::try_from(arguments).expect("three arguments");
            found.push((edge0, edge1));
        }
        found
    }

    #[test]
    fn built_in_shaders_call_smoothstep_only_with_ascending_edges() {
        // naga emits MSL/GLSL `smoothstep` and SPIR-V `SmoothStep`, which leave `edge0 >= edge1`
        // undefined; descending ramps go through `builtin_smoothstep`. Each raw call below is
        // ascending by its constants or its parameter's schema range.
        let ascending = [
            ("cross-warp.wgsl", "0.0", "1.0", "constants"),
            ("ripple.wgsl", "0.2", "1.0", "constants"),
            ("window-blinds.wgsl", "0.8", "1.0", "constants"),
            (
                "simple-zoom.wgsl",
                "0.0",
                "quickness",
                "quickness is clamped to 0.2..1",
            ),
            (
                "simple-zoom.wgsl",
                "quickness-0.2",
                "1.0",
                "quickness is clamped to 0.2..1",
            ),
            (
                "directional-warp.wgsl",
                "-params.smoothness",
                "0.0",
                "smoothness >= 0.0001",
            ),
            (
                "crop.wgsl",
                "line-feather",
                "line+feather",
                "feather >= 0.0001, line in 0..1",
            ),
            (
                "vignette.wgsl",
                "params.radius-params.softness",
                "params.radius+params.softness",
                "softness >= 0.01, radius <= 1.5",
            ),
            // threshold <= sqrt(3) < 2, where f32 values are f32::EPSILON apart, and
            // softness >= f32::EPSILON: the sum is at least the next value up.
            (
                "chroma-key.wgsl",
                "params.threshold",
                "params.threshold+params.softness",
                "softness >= f32::EPSILON, threshold <= sqrt(3)",
            ),
        ];
        let mut files = Vec::new();
        shader_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("shaders"),
            &mut files,
        );
        let mut used = vec![false; ascending.len()];
        for file in files {
            let name = file
                .file_name()
                .and_then(|name| name.to_str())
                .expect("name");
            let text = std::fs::read_to_string(&file).expect("shader text");
            for (edge0, edge1) in smoothstep_edges(&text) {
                let listed = ascending.iter().position(|&(listed, low, high, _)| {
                    (listed, low, high) == (name, edge0.as_str(), edge1.as_str())
                });
                let Some(index) = listed else {
                    panic!(
                        "{}: smoothstep({edge0}, {edge1}, ...) is not known to be ascending; \
                         call builtin_smoothstep instead",
                        file.display()
                    );
                };
                used[index] = true;
            }
        }
        for (index, entry) in ascending.iter().enumerate() {
            assert!(used[index], "no longer called: {entry:?}");
        }
    }
}
