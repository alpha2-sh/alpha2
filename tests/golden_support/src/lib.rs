//! PNG goldens under `tests/golden`, shared by alpha2's GPU test suites.
//!
//! A golden has one or more variants, each scoped to the runs it describes (a backend, an
//! adapter, an intermediate). A run is compared only with the variants whose scope covers it,
//! and an update writes only into those, so one backend's rendering never loosens another's.
//! Each variant has a `<variant>.provenance.json` sidecar recording which run drew each tile.
//!
//! `ALPHA2_UPDATE_GOLDENS=1` rewrites a golden instead of comparing; any other value, `0` and
//! empty included, compares. `ALPHA2_GOLDEN_ACTUAL_DIR` keeps a mismatching render for
//! inspection, named after the run that drew it. `ALPHA2_GOLDEN_VARIANT=<name>` directs an
//! update of a tiled golden into that variant alone; it must be in the run's scope.
//! `ALPHA2_GOLDEN_REPORT=1` prints which variant each tile matched. `ALPHA2_GOLDEN_SIDECARS=init`
//! writes a sidecar with every tile unrecorded for a variant that has none, while comparing.

use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
};

/// The run a rendering comes from, as goldens scope and record it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoldenRun {
    /// `gpu` or `cpu`.
    pub renderer: &'static str,
    /// The adapter's name.
    pub adapter: String,
    /// The backend (`Vulkan`, `Metal`, `Dx12`, `Gl`; `cpu` for the CPU renderer).
    pub backend: String,
    /// The driver name and version.
    pub driver: String,
    /// The DX12 shader compiler; empty elsewhere.
    pub shader_compiler: String,
    /// The intermediate format (`Rgba16Float`, `Rgba8Unorm`).
    pub intermediate: String,
    /// Whether the adapter rasterizes on the CPU.
    pub software: bool,
    /// `debug` or `release`.
    pub profile: &'static str,
}

impl GoldenRun {
    /// A run of the CPU renderer, which stores half floats and draws the same pictures everywhere.
    #[must_use]
    pub fn cpu() -> Self {
        Self {
            renderer: "cpu",
            adapter: "alpha2 CPU renderer".to_owned(),
            backend: "cpu".to_owned(),
            driver: "alpha2".to_owned(),
            shader_compiler: String::new(),
            intermediate: "Rgba16Float".to_owned(),
            software: false,
            profile: Self::profile(),
        }
    }

    /// The build profile this test binary was compiled in.
    #[must_use]
    pub const fn profile() -> &'static str {
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    }

    fn provenance(&self) -> Provenance {
        Provenance::Recorded {
            adapter: self.adapter.clone(),
            backend: self.backend.clone(),
            driver: self.driver.clone(),
            shader_compiler: self.shader_compiler.clone(),
            intermediate: self.intermediate.clone(),
            renderer: self.renderer.to_owned(),
            profile: self.profile.to_owned(),
            stale: false,
        }
    }
}

/// Which runs a variant describes; an unset field matches every run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    /// The backend, compared ignoring case.
    pub backend: Option<&'static str>,
    /// Text the adapter's name contains, ignoring case.
    pub adapter_contains: Option<&'static str>,
    /// The intermediate format, compared ignoring case.
    pub intermediate: Option<&'static str>,
}

impl Scope {
    /// Every run.
    pub const EVERYWHERE: Self = Self {
        backend: None,
        adapter_contains: None,
        intermediate: None,
    };

    /// Whether this scope describes `run`.
    #[must_use]
    pub fn covers(&self, run: &GoldenRun) -> bool {
        self.backend
            .is_none_or(|backend| backend.eq_ignore_ascii_case(&run.backend))
            && self
                .adapter_contains
                .is_none_or(|text| run.adapter.to_lowercase().contains(&text.to_lowercase()))
            && self
                .intermediate
                .is_none_or(|format| format.eq_ignore_ascii_case(&run.intermediate))
    }

    /// How many fields narrow the scope: the most specific in-scope variant takes a change.
    fn specificity(&self) -> usize {
        usize::from(self.backend.is_some())
            + usize::from(self.adapter_contains.is_some())
            + usize::from(self.intermediate.is_some())
    }

    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(backend) = self.backend {
            parts.push(format!("backend {backend}"));
        }
        if let Some(text) = self.adapter_contains {
            parts.push(format!("an adapter named like {text}"));
        }
        if let Some(format) = self.intermediate {
            parts.push(format!("the {format} intermediate"));
        }
        if parts.is_empty() {
            "every run".to_owned()
        } else {
            parts.join(", ")
        }
    }
}

/// One variant of a golden: its file name (without `.png`) and the runs it describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Variant {
    /// The PNG's file name in `tests/golden`, without `.png`.
    pub name: &'static str,
    /// The runs it describes.
    pub scope: Scope,
}

impl Variant {
    /// A variant every run is compared with.
    #[must_use]
    pub const fn everywhere(name: &'static str) -> Self {
        Self {
            name,
            scope: Scope::EVERYWHERE,
        }
    }
}

/// The label of a golden that is not tiled, in its sidecar.
const WHOLE: &str = "whole";

/// Compares tightly packed RGBA8 `actual` of `size` (width, height) from `run` with the golden
/// `name`, each byte within `tolerance`.
///
/// # Panics
/// Panics on a mismatch, or when the golden is missing or malformed.
pub fn assert_rgba8_golden(
    run: &GoldenRun,
    name: &'static str,
    size: (u32, u32),
    actual: &[u8],
    tolerance: u8,
) {
    assert_rgba8_golden_tiles(
        run,
        &[Variant::everywhere(name)],
        size,
        &[WHOLE],
        actual,
        tolerance,
    );
}

/// Like [`assert_rgba8_golden`] for an image of side-by-side tiles, one per label in `tiles`,
/// each as wide as the image divided among them. Each tile passes when it matches that tile of a
/// variant whose scope covers `run` and whose copy of the tile is not stale.
///
/// An update leaves a tile that still matches such a variant as it is. A tile that matches none
/// changed: the most specific in-scope variant (or the one `ALPHA2_GOLDEN_VARIANT` names, which
/// must be in scope) takes the new rendering, and every other variant's different copy of that
/// tile is marked stale in its sidecar, so no older rendering stays accepted until a run in that
/// variant's scope draws it again. An in-scope variant that is missing, or of another size, is
/// written whole; an out-of-scope one is never written.
///
/// # Panics
/// Panics when a tile matches no variant, when the width does not divide among the tiles, or
/// when a golden is missing or malformed.
pub fn assert_rgba8_golden_tiles(
    run: &GoldenRun,
    variants: &[Variant],
    size: (u32, u32),
    tiles: &[&str],
    actual: &[u8],
    tolerance: u8,
) {
    let first = variants.first().expect("at least one golden variant");
    let golden = Tiled {
        directory: golden_directory(),
        variants,
        size,
        tiles,
        tolerance,
    };
    if update_requested(std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref()) {
        let forced = std::env::var("ALPHA2_GOLDEN_VARIANT")
            .ok()
            .filter(|variant| !variant.is_empty());
        for note in golden.update(run, actual, forced.as_deref()) {
            eprintln!("{note}");
        }
        return;
    }
    if std::env::var("ALPHA2_GOLDEN_SIDECARS").as_deref() == Ok("init") {
        golden.initialize_sidecars();
    }
    let outcome = golden.compare(run, actual);
    if std::env::var_os("ALPHA2_GOLDEN_REPORT").as_deref() == Some(OsStr::new("1")) {
        for line in &outcome.matched {
            eprintln!("golden {}: {line}", first.name);
        }
    }
    if !outcome.failures.is_empty() {
        keep_actual(run, first.name, size, actual);
    }
    assert!(
        outcome.failures.is_empty(),
        "golden mismatch for every variant in this run's scope in {}; rerun with \
         ALPHA2_UPDATE_GOLDENS=1 to accept an intended change",
        outcome.failures.join("; ")
    );
}

/// Who drew one tile of a variant.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Provenance {
    /// From before provenance was recorded: accepted until its next update.
    Unrecorded,
    Recorded {
        adapter: String,
        backend: String,
        driver: String,
        shader_compiler: String,
        intermediate: String,
        renderer: String,
        profile: String,
        /// The tile changed on another scope's run; this copy is not accepted until a run in
        /// this variant's scope draws it again.
        stale: bool,
    },
}

impl Provenance {
    fn stale(&self) -> bool {
        matches!(self, Self::Recorded { stale: true, .. })
    }

    fn mark_stale(&mut self) {
        match self {
            Self::Recorded { stale, .. } => *stale = true,
            Self::Unrecorded => {
                *self = Self::Recorded {
                    adapter: String::new(),
                    backend: String::new(),
                    driver: String::new(),
                    shader_compiler: String::new(),
                    intermediate: String::new(),
                    renderer: String::new(),
                    profile: String::new(),
                    stale: true,
                };
            }
        }
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Unrecorded => serde_json::json!({ "unrecorded": true }),
            Self::Recorded {
                adapter,
                backend,
                driver,
                shader_compiler,
                intermediate,
                renderer,
                profile,
                stale,
            } => serde_json::json!({
                "adapter": adapter,
                "backend": backend,
                "driver": driver,
                "intermediate": intermediate,
                "profile": profile,
                "renderer": renderer,
                "shaderCompiler": shader_compiler,
                "stale": stale,
            }),
        }
    }

    fn from_json(value: &serde_json::Value) -> Self {
        if value["unrecorded"] == true {
            return Self::Unrecorded;
        }
        let text = |key: &str| value[key].as_str().unwrap_or_default().to_owned();
        Self::Recorded {
            adapter: text("adapter"),
            backend: text("backend"),
            driver: text("driver"),
            shader_compiler: text("shaderCompiler"),
            intermediate: text("intermediate"),
            renderer: text("renderer"),
            profile: text("profile"),
            stale: value["stale"] == true,
        }
    }
}

/// A variant's sidecar: the provenance of each tile, by label.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Sidecar(BTreeMap<String, Provenance>);

impl Sidecar {
    fn path(directory: &Path, name: &str) -> PathBuf {
        directory.join(format!("{name}.provenance.json"))
    }

    /// The sidecar of `name`; a missing one records every tile as unrecorded.
    fn read(directory: &Path, name: &str) -> Self {
        let Ok(text) = std::fs::read_to_string(Self::path(directory, name)) else {
            return Self::default();
        };
        let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_else(|error| {
            panic!("golden sidecar {name}.provenance.json is not JSON: {error}")
        });
        Self(
            value["tiles"]
                .as_object()
                .map(|tiles| {
                    tiles
                        .iter()
                        .map(|(label, entry)| (label.clone(), Provenance::from_json(entry)))
                        .collect()
                })
                .unwrap_or_default(),
        )
    }

    fn tile(&self, label: &str) -> Provenance {
        self.0.get(label).cloned().unwrap_or(Provenance::Unrecorded)
    }

    fn write(&self, directory: &Path, name: &str) {
        let tiles: serde_json::Map<String, serde_json::Value> = self
            .0
            .iter()
            .map(|(label, provenance)| (label.clone(), provenance.to_json()))
            .collect();
        let mut text = serde_json::to_string_pretty(&serde_json::json!({ "tiles": tiles }))
            .expect("a sidecar serializes");
        text.push('\n');
        std::fs::write(Self::path(directory, name), text).expect("golden sidecar can be written");
    }
}

/// A tiled golden: `variants` of an image of `tiles` side by side.
struct Tiled<'a> {
    directory: PathBuf,
    variants: &'a [Variant],
    size: (u32, u32),
    tiles: &'a [&'a str],
    tolerance: u8,
}

/// What a comparison found: one line per tile that matched no variant, and one per tile that did.
struct Outcome {
    failures: Vec<String>,
    matched: Vec<String>,
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

    /// The first byte of tile `tile` where `expected` differs from `actual` by more than the
    /// tolerance.
    fn tile_mismatch(&self, tile: usize, actual: &[u8], expected: &[u8]) -> Option<usize> {
        self.tile_bytes(tile)
            .find(|&index| actual[index].abs_diff(expected[index]) > self.tolerance)
    }

    fn in_scope<'v>(&'v self, run: &GoldenRun) -> Vec<&'v Variant> {
        self.variants
            .iter()
            .filter(|variant| variant.scope.covers(run))
            .collect()
    }

    fn compare(&self, run: &GoldenRun, actual: &[u8]) -> Outcome {
        let consulted = self.in_scope(run);
        assert!(
            !consulted.is_empty(),
            "no variant of {} covers this run ({} on {}, {}); add one scoped to it",
            self.variants[0].name,
            run.adapter,
            run.backend,
            run.intermediate
        );
        let loaded = consulted
            .iter()
            .map(|variant| {
                let path = self.path(variant.name);
                let expected = read_png(&path, self.size);
                assert_eq!(actual.len(), expected.len(), "{}", path.display());
                (
                    *variant,
                    expected,
                    Sidecar::read(&self.directory, variant.name),
                )
            })
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        let mut matched = Vec::new();
        for (tile, label) in self.tiles.iter().enumerate() {
            let mut tried = Vec::new();
            let mut hit = None;
            for (variant, expected, sidecar) in &loaded {
                if sidecar.tile(label).stale() {
                    tried.push(format!("{} is stale for this tile", variant.name));
                    continue;
                }
                match self.tile_mismatch(tile, actual, expected) {
                    None => {
                        hit = Some(variant.name);
                        break;
                    }
                    Some(byte) => tried.push(format!("{} at byte {byte}", variant.name)),
                }
            }
            match hit {
                Some(name) => matched.push(format!("tile {tile} ({label}) matched {name}")),
                None => failures.push(format!("tile {tile} ({label}): {}", tried.join(", "))),
            }
        }
        Outcome { failures, matched }
    }

    /// Writes a sidecar with every tile unrecorded for each variant that has none.
    fn initialize_sidecars(&self) {
        for variant in self.variants {
            if Sidecar::path(&self.directory, variant.name).exists()
                || !self.path(variant.name).exists()
            {
                continue;
            }
            Sidecar(
                self.tiles
                    .iter()
                    .map(|label| ((*label).to_owned(), Provenance::Unrecorded))
                    .collect(),
            )
            .write(&self.directory, variant.name);
        }
    }

    /// The variant an update from `run` writes changed tiles into: `forced`, which must cover
    /// the run, else the most specific variant that does (the first listed of equals).
    fn update_target(&self, run: &GoldenRun, forced: Option<&str>) -> &Variant {
        let in_scope = self.in_scope(run);
        match forced {
            Some(name) => {
                let variant = self
                    .variants
                    .iter()
                    .find(|variant| variant.name == name)
                    .unwrap_or_else(|| {
                        panic!(
                            "ALPHA2_GOLDEN_VARIANT={name} names none of the variants {:?}",
                            self.variants
                                .iter()
                                .map(|variant| variant.name)
                                .collect::<Vec<_>>()
                        )
                    });
                assert!(
                    variant.scope.covers(run),
                    "ALPHA2_GOLDEN_VARIANT={name} describes {}, not this run ({} on {}, {})",
                    variant.scope.describe(),
                    run.adapter,
                    run.backend,
                    run.intermediate
                );
                variant
            }
            None => in_scope
                .iter()
                .rev()
                .max_by_key(|variant| variant.scope.specificity())
                .unwrap_or_else(|| {
                    panic!(
                        "no variant of {} covers this run ({} on {}); add one scoped to it",
                        self.variants[0].name, run.adapter, run.backend
                    )
                }),
        }
    }

    /// Rewrites the variants for `actual` from `run`, as [`assert_rgba8_golden_tiles`]
    /// describes. Returns what it rewrote and what it marked stale.
    fn update(&self, run: &GoldenRun, actual: &[u8], forced: Option<&str>) -> Vec<String> {
        let target = self.update_target(run, forced);
        let mut notes = Vec::new();
        let mut state = self
            .variants
            .iter()
            .map(|variant| {
                let path = self.path(variant.name);
                let current = read_png_if_sized(&path, self.size)
                    .filter(|pixels| pixels.len() == actual.len());
                let sidecar = Sidecar::read(&self.directory, variant.name);
                (*variant, path, current, sidecar, false)
            })
            .collect::<Vec<_>>();
        for (variant, path, current, sidecar, changed) in &mut state {
            let writable = variant.scope.covers(run) && (forced.is_none() || *variant == *target);
            if current.is_none() && writable {
                notes.push(format!(
                    "golden {} was missing or of another size: written whole",
                    path.display()
                ));
                *current = Some(actual.to_vec());
                for label in self.tiles {
                    sidecar.0.insert((*label).to_owned(), run.provenance());
                }
                *changed = true;
            }
        }
        for (tile, label) in self.tiles.iter().enumerate() {
            let accepted = state.iter().any(|(variant, _, current, sidecar, _)| {
                variant.scope.covers(run)
                    && !sidecar.tile(label).stale()
                    && current.as_ref().is_some_and(|expected| {
                        self.tile_mismatch(tile, actual, expected).is_none()
                    })
            });
            if accepted {
                continue;
            }
            for (variant, path, current, sidecar, changed) in &mut state {
                let Some(pixels) = current.as_mut() else {
                    continue;
                };
                if *variant == *target {
                    for byte in self.tile_bytes(tile) {
                        pixels[byte] = actual[byte];
                    }
                    sidecar.0.insert((*label).to_owned(), run.provenance());
                    *changed = true;
                } else if self.tile_mismatch(tile, actual, pixels).is_some()
                    && !sidecar.tile(label).stale()
                {
                    let mut provenance = sidecar.tile(label);
                    provenance.mark_stale();
                    sidecar.0.insert((*label).to_owned(), provenance);
                    *changed = true;
                    notes.push(format!(
                        "tile {tile} ({label}) changed: its copy in {} is stale until a run on {} draws it again (ALPHA2_GOLDEN_VARIANT={})",
                        path.display(),
                        variant.scope.describe(),
                        variant.name
                    ));
                }
            }
        }
        for (variant, path, current, sidecar, changed) in &mut state {
            if *changed {
                if let Some(pixels) = current {
                    write_png(path, self.size, pixels);
                }
                // Every tile has an entry: one nobody redrew keeps what it had, or none.
                for label in self.tiles {
                    sidecar
                        .0
                        .entry((*label).to_owned())
                        .or_insert(Provenance::Unrecorded);
                }
                sidecar.write(&self.directory, variant.name);
            }
        }
        notes
    }
}

/// Whether `ALPHA2_UPDATE_GOLDENS` asks for a rewrite: only `1` does, so a switch set to `0`,
/// `false` or nothing still compares.
fn update_requested(value: Option<&OsStr>) -> bool {
    value == Some(OsStr::new("1"))
}

/// Decodes the RGBA8 golden at `path`, which must be `size`.
fn read_png(path: &Path, size: (u32, u32)) -> Vec<u8> {
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
fn read_png_if_sized(path: &Path, size: (u32, u32)) -> Option<Vec<u8>> {
    let file = File::open(path).ok()?;
    let reader = png::Decoder::new(BufReader::new(file)).read_info().ok()?;
    let info = reader.info();
    ((info.width, info.height) == size).then(|| read_png(path, size))
}

/// The file a mismatching render of `name` from `run` is kept as, so the renders of two runs
/// (backends, intermediates, renderers) never overwrite each other.
fn actual_file_name(run: &GoldenRun, name: &str) -> String {
    format!(
        "{name}.{}.{}.{}.png",
        run.backend.to_lowercase(),
        run.intermediate.to_lowercase(),
        run.renderer
    )
}

/// Saves a mismatch to `ALPHA2_GOLDEN_ACTUAL_DIR` for inspection across backends.
fn keep_actual(run: &GoldenRun, name: &str, size: (u32, u32), actual: &[u8]) {
    if let Some(directory) = std::env::var_os("ALPHA2_GOLDEN_ACTUAL_DIR") {
        let directory = PathBuf::from(directory);
        std::fs::create_dir_all(&directory).expect("golden actual directory can be created");
        write_png(&directory.join(actual_file_name(run, name)), size, actual);
    }
}

fn golden_directory() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../golden")
}

fn write_png(path: &Path, size: (u32, u32), pixels: &[u8]) {
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

    fn run(backend: &str, adapter: &str) -> GoldenRun {
        GoldenRun {
            renderer: "gpu",
            adapter: adapter.to_owned(),
            backend: backend.to_owned(),
            driver: "test".to_owned(),
            shader_compiler: String::new(),
            intermediate: "Rgba16Float".to_owned(),
            software: false,
            profile: "debug",
        }
    }

    const VULKAN: Variant = Variant::everywhere("first");
    const DX12: Variant = Variant {
        name: "alternative",
        scope: Scope {
            backend: Some("Dx12"),
            adapter_contains: None,
            intermediate: None,
        },
    };

    fn tiled<'a>(directory: &Path, variants: &'a [Variant]) -> Tiled<'a> {
        Tiled {
            directory: directory.to_path_buf(),
            variants,
            size: (4, 1),
            tiles: &["left", "right"],
            tolerance: 0,
        }
    }

    #[test]
    fn a_variant_scoped_to_another_backend_is_not_consulted() {
        let directory = scratch("scoped");
        let variants = [VULKAN, DX12];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));
        // The DX12 variant's rendering of the right tile passes a DX12 run only.
        let vulkan = run("Vulkan", "NVIDIA GeForce RTX 4090");
        let dx12 = run("Dx12", "NVIDIA GeForce MX450");
        assert_eq!(golden.compare(&dx12, &image(10, 25)).failures.len(), 0);
        let failures = golden.compare(&vulkan, &image(10, 25)).failures;
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("tile 1 (right)"), "{failures:?}");
        assert!(!failures[0].contains("alternative"), "{failures:?}");
    }

    #[test]
    fn an_update_never_writes_an_out_of_scope_variant() {
        let directory = scratch("out-of-scope");
        let variants = [VULKAN, DX12];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));
        golden.update(&run("Vulkan", "llvmpipe"), &image(90, 20), None);
        assert_eq!(
            read_png(&directory.join("first.png"), (4, 1)),
            image(90, 20)
        );
        assert_eq!(
            read_png(&directory.join("alternative.png"), (4, 1)),
            image(10, 25),
            "the DX12 variant's pixels are untouched"
        );
    }

    #[test]
    fn a_changed_tile_marks_out_of_scope_copies_stale_and_they_stop_matching() {
        let directory = scratch("stale");
        let variants = [VULKAN, DX12];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));
        let notes = golden.update(&run("Vulkan", "RTX"), &image(90, 20), None);
        assert!(
            notes
                .iter()
                .any(|note| note.contains("tile 0 (left) changed")
                    && note.contains("alternative.png is stale")),
            "{notes:?}"
        );
        // The DX12 run's old left tile is no longer accepted, nor its right tile's: only the
        // changed tile is stale.
        let dx12 = run("Dx12", "MX450");
        let failures = golden.compare(&dx12, &image(10, 25)).failures;
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].contains("stale for this tile"), "{failures:?}");
        // A DX12 run draws it again and clears the mark.
        golden.update(&dx12, &image(91, 25), None);
        assert_eq!(
            golden.compare(&dx12, &image(91, 25)).failures,
            [] as [String; 0]
        );
    }

    #[test]
    fn an_update_records_who_drew_each_tile() {
        let directory = scratch("provenance");
        let variants = [VULKAN];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        golden.update(&run("Vulkan", "RTX 4090"), &image(10, 30), None);
        let sidecar: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("first.provenance.json")).expect("sidecar"),
        )
        .expect("JSON");
        assert_eq!(sidecar["tiles"]["right"]["adapter"], "RTX 4090");
        assert_eq!(sidecar["tiles"]["right"]["backend"], "Vulkan");
        assert_eq!(sidecar["tiles"]["right"]["stale"], false);
        // The unchanged tile was drawn before provenance and stays so.
        assert_eq!(sidecar["tiles"]["left"]["unrecorded"], true);
    }

    #[test]
    fn a_forced_variant_outside_the_runs_scope_is_refused() {
        let directory = scratch("forced");
        let variants = [VULKAN, DX12];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));
        let refused = std::panic::catch_unwind(|| {
            golden.update(&run("Vulkan", "RTX"), &image(1, 2), Some("alternative"));
        })
        .expect_err("a Vulkan run cannot write the DX12 variant");
        let message = refused.downcast_ref::<String>().expect("a message");
        assert!(message.contains("describes backend Dx12"), "{message}");
        // In its scope it takes the change alone.
        golden.update(&run("Dx12", "MX450"), &image(10, 26), Some("alternative"));
        assert_eq!(
            read_png(&directory.join("first.png"), (4, 1)),
            image(10, 20)
        );
        assert_eq!(
            read_png(&directory.join("alternative.png"), (4, 1)),
            image(10, 26)
        );
    }

    #[test]
    fn actuals_of_two_runs_do_not_overwrite_each_other() {
        let mut eight_bit = run("Vulkan", "RTX");
        eight_bit.intermediate = "Rgba8Unorm".to_owned();
        let names = [
            actual_file_name(&run("Vulkan", "RTX"), "glow"),
            actual_file_name(&eight_bit, "glow"),
            actual_file_name(&run("Dx12", "MX450"), "glow"),
            actual_file_name(&GoldenRun::cpu(), "glow"),
        ];
        assert_eq!(names[0], "glow.vulkan.rgba16float.gpu.png");
        assert_eq!(names[3], "glow.cpu.rgba16float.cpu.png");
        let distinct: std::collections::BTreeSet<_> = names.iter().collect();
        assert_eq!(distinct.len(), names.len());
    }

    #[test]
    fn an_update_that_still_matches_a_variant_rewrites_nothing() {
        let directory = scratch("still-matches");
        let variants = [VULKAN, Variant::everywhere("alternative")];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (4, 1), &image(10, 20));
        write_png(&directory.join("alternative.png"), (4, 1), &image(10, 25));
        assert_eq!(
            golden.update(&run("Vulkan", "RTX"), &image(10, 25), None),
            [] as [String; 0]
        );
        assert!(!directory.join("first.provenance.json").exists());
        let vulkan = run("Vulkan", "RTX");
        assert_eq!(
            golden.compare(&vulkan, &image(10, 20)).failures,
            [] as [String; 0]
        );
        assert_eq!(
            golden.compare(&vulkan, &image(10, 25)).failures,
            [] as [String; 0]
        );
    }

    #[test]
    fn an_update_rewrites_an_in_scope_variant_of_another_size_whole() {
        let directory = scratch("resized");
        let variants = [VULKAN];
        let golden = tiled(&directory, &variants);
        write_png(&directory.join("first.png"), (2, 1), &image(10, 20)[..8]);
        golden.update(&run("Vulkan", "RTX"), &image(10, 20), None);
        assert_eq!(
            read_png(&directory.join("first.png"), (4, 1)),
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
