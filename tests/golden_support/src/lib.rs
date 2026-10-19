//! PNG goldens under `tests/golden`, shared by alpha2's GPU test suites.
//!
//! `ALPHA2_UPDATE_GOLDENS=1` rewrites a golden instead of comparing; any
//! other value, `0` and empty included, compares.
//! `ALPHA2_GOLDEN_ACTUAL_DIR` keeps a mismatching render for inspection.
//! `ALPHA2_GOLDEN_VARIANT=<name>` directs an update of a tiled golden into
//! that variant alone, to add one backend's rendering of a changed tile.

use std::{
    ffi::OsStr,
    fs::File,
    io::{BufReader, BufWriter},
    path::PathBuf,
};

/// Compares tightly packed RGBA8 `actual` of `size` (width, height) with the
/// golden `name`, each byte within `tolerance`.
///
/// # Panics
/// Panics on a mismatch, or when the golden is missing or malformed.
pub fn assert_rgba8_golden(name: &str, size: (u32, u32), actual: &[u8], tolerance: u8) {
    let path = golden_path(name);
    if update_requested(std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref()) {
        write_png(&path, size, actual);
        return;
    }

    let mismatch = rgba8_golden_mismatch(&path, size, actual, tolerance);
    if mismatch.is_some() {
        keep_actual(name, size, actual);
    }
    assert!(
        mismatch.is_none(),
        "golden mismatch in {} at byte {:?} with tolerance {tolerance}; rerun with ALPHA2_UPDATE_GOLDENS=1 to accept an intended change",
        path.display(),
        mismatch
    );
}

/// Like [`assert_rgba8_golden`] for an image of side-by-side tiles, one per
/// label in `tiles`, each as wide as the image divided among them. Each tile
/// passes when it matches that tile of any reviewed variant in `names`, so
/// backends that differ in unrelated tiles need no variant for every
/// combination.
///
/// An update leaves a tile that still matches some variant as it is. A tile
/// that matches none changed: the first variant takes the new rendering,
/// and so does every other variant, so no older rendering of that tile stays
/// accepted (each dropped alternative is printed). A backend that draws a
/// changed tile differently then fails until its own rendering is added with
/// `ALPHA2_GOLDEN_VARIANT=<name>`, which writes changed tiles into that
/// variant only. A variant that is missing, or of another size, is written
/// whole.
///
/// # Panics
/// Panics when a tile matches no variant, when the width does not divide
/// among the tiles, or when a golden is missing or malformed.
pub fn assert_rgba8_golden_tiles(
    names: &[&str],
    size: (u32, u32),
    tiles: &[&str],
    actual: &[u8],
    tolerance: u8,
) {
    let first = names.first().expect("at least one golden variant");
    let update =
        update_requested(std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref()).then(|| {
            std::env::var("ALPHA2_GOLDEN_VARIANT")
                .ok()
                .filter(|variant| !variant.is_empty())
        });
    let golden = Tiled {
        directory: golden_directory(),
        names,
        size,
        tiles,
        tolerance,
    };
    if let Some(variant) = update {
        for note in golden.update(actual, variant.as_deref()) {
            eprintln!("{note}");
        }
        return;
    }
    let failures = golden.mismatches(actual);
    if !failures.is_empty() {
        keep_actual(first, size, actual);
    }
    assert!(
        failures.is_empty(),
        "golden mismatch for every reviewed variant in {}; rerun with ALPHA2_UPDATE_GOLDENS=1 to accept an intended change",
        failures.join("; ")
    );
}

/// A tiled golden: variants `names` of an image of `tiles` side by side.
struct Tiled<'a> {
    directory: PathBuf,
    names: &'a [&'a str],
    size: (u32, u32),
    tiles: &'a [&'a str],
    tolerance: u8,
}

impl Tiled<'_> {
    fn path(&self, name: &str) -> PathBuf {
        self.directory.join(format!("{name}.png"))
    }

    /// The byte indices of tile `tile`, row by row.
    fn tile_bytes(&self, tile: usize) -> impl Iterator<Item = usize> {
        let tile_count = u32::try_from(self.tiles.len()).expect("tile count fits u32");
        assert!(
            tile_count > 0 && self.size.0.is_multiple_of(tile_count),
            "{} px do not divide among {} tiles",
            self.size.0,
            self.tiles.len()
        );
        let tile_bytes = (self.size.0 / tile_count) as usize * 4;
        let row_bytes = self.size.0 as usize * 4;
        (0..self.size.1 as usize).flat_map(move |row| {
            let start = row * row_bytes + tile * tile_bytes;
            start..start + tile_bytes
        })
    }

    /// The first byte of tile `tile` where `expected` differs from `actual`
    /// by more than the tolerance.
    fn tile_mismatch(&self, tile: usize, actual: &[u8], expected: &[u8]) -> Option<usize> {
        self.tile_bytes(tile)
            .find(|&index| actual[index].abs_diff(expected[index]) > self.tolerance)
    }

    /// One line for each tile that matches no variant.
    fn mismatches(&self, actual: &[u8]) -> Vec<String> {
        let variants = self
            .names
            .iter()
            .map(|name| {
                let path = self.path(name);
                let expected = read_png(&path, self.size);
                assert_eq!(actual.len(), expected.len(), "{}", path.display());
                (path, expected)
            })
            .collect::<Vec<_>>();
        self.tiles
            .iter()
            .enumerate()
            .filter_map(|(tile, label)| {
                let mismatches = variants
                    .iter()
                    .map(|(path, expected)| (path, self.tile_mismatch(tile, actual, expected)))
                    .collect::<Vec<_>>();
                if mismatches.iter().any(|(_, mismatch)| mismatch.is_none()) {
                    return None;
                }
                Some(format!(
                    "tile {tile} ({label}): {}",
                    mismatches
                        .iter()
                        .map(|(path, mismatch)| format!("{} at byte {mismatch:?}", path.display()))
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })
            .collect()
    }

    /// Rewrites the variants for `actual`, as [`assert_rgba8_golden_tiles`]
    /// describes, into the variant named `only` alone when one is given.
    /// Returns what it rewrote and what it dropped.
    fn update(&self, actual: &[u8], only: Option<&str>) -> Vec<String> {
        if let Some(only) = only {
            assert!(
                self.names.contains(&only),
                "ALPHA2_GOLDEN_VARIANT={only} names none of the variants {:?}",
                self.names
            );
        }
        let mut notes = Vec::new();
        let mut variants = self
            .names
            .iter()
            .map(|name| {
                let path = self.path(name);
                let current = read_png_if_sized(&path, self.size)
                    .filter(|pixels| pixels.len() == actual.len());
                (*name, path, current)
            })
            .collect::<Vec<_>>();
        let mut changed = vec![false; variants.len()];
        for (index, (name, path, current)) in variants.iter_mut().enumerate() {
            if current.is_none() && only.is_none_or(|only| only == *name) {
                notes.push(format!(
                    "golden {} was missing or of another size: written whole",
                    path.display()
                ));
                *current = Some(actual.to_vec());
                changed[index] = true;
            }
        }
        for (tile, label) in self.tiles.iter().enumerate() {
            let accepted = variants.iter().any(|(_, _, current)| {
                current
                    .as_ref()
                    .is_some_and(|expected| self.tile_mismatch(tile, actual, expected).is_none())
            });
            if accepted {
                continue;
            }
            for (index, (name, path, current)) in variants.iter_mut().enumerate() {
                let Some(pixels) = current.as_mut() else {
                    continue;
                };
                if only.is_some_and(|only| only != *name) {
                    continue;
                }
                if index > 0 && only.is_none() {
                    notes.push(format!(
                        "tile {tile} ({label}) changed: its alternative in {} is dropped; add it again with ALPHA2_GOLDEN_VARIANT={name} on the backend that draws it",
                        path.display()
                    ));
                }
                for byte in self.tile_bytes(tile) {
                    pixels[byte] = actual[byte];
                }
                changed[index] = true;
            }
        }
        for ((_, path, current), changed) in variants.iter().zip(changed) {
            if let (true, Some(pixels)) = (changed, current) {
                write_png(path, self.size, pixels);
            }
        }
        notes
    }
}

/// Whether `ALPHA2_UPDATE_GOLDENS` asks for a rewrite: only `1` does, so a
/// switch set to `0`, `false` or nothing still compares.
fn update_requested(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

fn rgba8_golden_mismatch(
    path: &PathBuf,
    size: (u32, u32),
    actual: &[u8],
    tolerance: u8,
) -> Option<usize> {
    let expected = read_png(path, size);
    assert_eq!(actual.len(), expected.len());

    actual
        .iter()
        .zip(&expected)
        .enumerate()
        .find(|(_, (actual, expected))| actual.abs_diff(**expected) > tolerance)
        .map(|(index, _)| index)
}

/// Decodes the RGBA8 golden at `path`, which must be `size`.
fn read_png(path: &PathBuf, size: (u32, u32)) -> Vec<u8> {
    let decoder = png::Decoder::new(BufReader::new(File::open(path).unwrap_or_else(|error| {
        panic!(
            "golden image {} is unavailable ({error}); regenerate with ALPHA2_UPDATE_GOLDENS=1",
            path.display()
        )
    })));
    let mut reader = decoder.read_info().expect("golden PNG header is valid");
    let mut expected = vec![
        0;
        reader
            .output_buffer_size()
            .expect("golden PNG fits in memory")
    ];
    let output = reader
        .next_frame(&mut expected)
        .expect("golden PNG pixels decode");
    expected.truncate(output.buffer_size());

    assert_eq!(
        (output.width, output.height),
        size,
        "golden image {} has another size; regenerate with ALPHA2_UPDATE_GOLDENS=1",
        path.display()
    );
    assert_eq!(
        (output.color_type, output.bit_depth),
        (png::ColorType::Rgba, png::BitDepth::Eight),
        "golden image {} is not RGBA8",
        path.display()
    );
    expected
}

/// The RGBA8 golden at `path` when it exists and is `size`.
fn read_png_if_sized(path: &PathBuf, size: (u32, u32)) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let reader = png::Decoder::new(BufReader::new(file)).read_info().ok()?;
    let info = reader.info();
    ((info.width, info.height) == size).then(|| read_png(path, size))
}

/// Saves mismatches to `ALPHA2_GOLDEN_ACTUAL_DIR` for inspection across backends.
fn keep_actual(name: &str, size: (u32, u32), actual: &[u8]) {
    if let Some(directory) = std::env::var_os("ALPHA2_GOLDEN_ACTUAL_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("golden actual directory can be created");
        write_png(&directory.join(format!("{name}.png")), size, actual);
    }
}

fn golden_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden")
}

fn golden_path(name: &str) -> PathBuf {
    golden_directory().join(format!("{name}.png"))
}

fn write_png(path: &PathBuf, size: (u32, u32), pixels: &[u8]) {
    assert_eq!(pixels.len(), (size.0 * size.1 * 4) as usize);
    let file = File::create(path).expect("golden PNG can be created");
    let mut encoder = png::Encoder::new(BufWriter::new(file), size.0, size.1);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .expect("golden PNG header can be written")
        .write_image_data(pixels)
        .expect("golden PNG pixels can be written");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn only_one_asks_for_a_rewrite() {
        assert!(update_requested(Some(OsStr::new("1"))));
        for value in ["0", "false", "", "true", "yes"] {
            assert!(!update_requested(Some(OsStr::new(value))), "{value:?}");
        }
        assert!(!update_requested(None));
    }

    /// A fresh directory for one test's goldens.
    fn scratch(test: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "alpha2-golden-support-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("scratch directory");
        directory
    }

    /// A 4x1 image of two tiles, each one grey level.
    fn image(left: u8, right: u8) -> Vec<u8> {
        [left, left, right, right]
            .iter()
            .flat_map(|&level| [level, level, level, 255])
            .collect()
    }

    fn tiled<'a>(directory: &Path, names: &'a [&'a str]) -> Tiled<'a> {
        Tiled {
            directory: directory.to_path_buf(),
            names,
            size: (4, 1),
            tiles: &["left", "right"],
            tolerance: 0,
        }
    }

    #[test]
    fn an_update_leaves_no_older_rendering_of_a_changed_tile_accepted() {
        let directory = scratch("changed-tile");
        let names = ["first", "alternative"];
        let golden = tiled(&directory, &names);
        // Reviewed: both variants agree on the left tile; the alternative draws the right tile
        // another way.
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));

        let notes = golden.update(&image(90, 20), None);
        assert_eq!(golden.mismatches(&image(90, 20)), [] as [String; 0]);
        assert!(
            golden.mismatches(&image(90, 25)).is_empty(),
            "an unchanged alternative stays"
        );
        let stale = golden.mismatches(&image(10, 20));
        assert_eq!(
            stale.len(),
            1,
            "the old left tile is accepted again: {stale:?}"
        );
        assert!(stale[0].starts_with("tile 0 (left)"), "{stale:?}");
        assert!(
            notes
                .iter()
                .any(|note| note.contains("tile 0 (left) changed")),
            "{notes:?}"
        );
    }

    #[test]
    fn an_update_that_still_matches_a_variant_rewrites_nothing() {
        let directory = scratch("still-matches");
        let names = ["first", "alternative"];
        let golden = tiled(&directory, &names);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));
        // The backend that draws the alternative updates: the first variant keeps its rendering.
        assert_eq!(golden.update(&image(10, 25), None), [] as [String; 0]);
        assert_eq!(golden.mismatches(&image(10, 20)), [] as [String; 0]);
        assert_eq!(golden.mismatches(&image(10, 25)), [] as [String; 0]);
    }

    #[test]
    fn a_named_variant_takes_one_backends_rendering_of_a_changed_tile() {
        let directory = scratch("named-variant");
        let names = ["first", "alternative"];
        let golden = tiled(&directory, &names);
        write_png(&directory.join("first.png"), (4, 1), &image(90, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(90, 20));
        golden.update(&image(95, 20), Some("alternative"));
        assert!(
            golden.mismatches(&image(90, 20)).is_empty(),
            "the first is untouched"
        );
        assert_eq!(golden.mismatches(&image(95, 20)), [] as [String; 0]);
    }

    #[test]
    fn an_update_rewrites_a_variant_of_another_size_whole() {
        let directory = scratch("resized");
        let names = ["first", "alternative"];
        let golden = tiled(&directory, &names);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(
            &directory.join("alternative.png"),
            (2, 1),
            &image(10, 20)[..8],
        );
        golden.update(&image(10, 20), None);
        assert_eq!(golden.mismatches(&image(10, 20)), [] as [String; 0]);
        assert_eq!(
            read_png(&directory.join("alternative.png"), (4, 1)),
            image(10, 20)
        );
    }

    #[test]
    fn a_golden_of_another_size_is_named() {
        let directory = scratch("named-size");
        write_png(&directory.join("small.png"), (2, 1), &image(10, 20)[..8]);
        let panic = std::panic::catch_unwind(|| read_png(&directory.join("small.png"), (4, 1)))
            .expect_err("another size panics");
        let message = panic.downcast_ref::<String>().expect("a formatted message");
        assert!(message.contains("small.png"), "{message}");
    }
}
