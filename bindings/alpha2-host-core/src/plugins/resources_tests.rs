use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use alpha2_engine::extract::fixtures::{digest_descriptor, video_input};
use alpha2_engine::extract::{PluginManifest, VideoFormat};
use alpha2_media::CancelToken;
use serde_json::{Value, json};

use super::resources::{MAX_SLOT_DEPTH, PinnedFile, ResourceReader, ResourceSet};
use super::{CANCELLED, PluginError, ResourceRoot, codes, sha256_hex, slot_reasons};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("alpha2-resources-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
    std::fs::write(path, bytes).expect("write");
}

fn manifest(resources: &Value) -> PluginManifest {
    let hash = "0".repeat(64);
    PluginManifest::from_json(
        &json!({
            "id": "alpha2.test.digest", "version": "1.0.0", "abi": "alpha2-plugin/1",
            "kind": "extractor",
            "artifacts": [{ "target": "wasm32", "type": "wasm", "url": "a.wasm", "sha256": hash, "bytes": 1 }],
            "resources": resources,
            "descriptor": digest_descriptor(video_input(VideoFormat::Rgba8, 64))
        })
        .to_string(),
    )
    .expect("manifest")
}

fn slots() -> PluginManifest {
    manifest(&json!([
        { "name": "model", "supplied": { "shape": "file" } },
        { "name": "extra", "supplied": { "shape": "directory", "required": false } }
    ]))
}

fn supplied(entries: &[(&str, &Path)]) -> BTreeMap<String, PathBuf> {
    entries
        .iter()
        .map(|(name, path)| ((*name).to_owned(), path.to_path_buf()))
        .collect()
}

fn resolve(
    manifest: &PluginManifest,
    entries: &[(&str, &Path)],
    root: &ResourceRoot,
) -> Result<ResourceSet, PluginError> {
    ResourceSet::resolve(manifest, &supplied(entries), root, false)
}

fn reason(error: &PluginError) -> (&'static str, Option<&str>, Option<&str>) {
    let detail = |key: &str| {
        error
            .details
            .as_ref()
            .and_then(|details| details.get(key))
            .and_then(Value::as_str)
    };
    (error.code, detail("slot"), detail("reason"))
}

fn files_of(set: &ResourceSet) -> Value {
    set.context()
}

#[test]
fn a_directory_slot_offers_its_files_by_relative_name_skipping_dot_entries() {
    let dir = scratch("names");
    let model = dir.join("model.bin");
    write(&model, b"weights");
    let extra = dir.join("extra");
    write(&extra.join("tokenizer.json"), b"{}");
    write(&extra.join("onnx/encoder.onnx"), b"enc");
    write(&extra.join(".cache/skip"), b"x");
    write(&extra.join(".DS_Store"), b"x");
    let set = resolve(
        &slots(),
        &[("model", &model), ("extra", &extra)],
        &ResourceRoot::Any,
    )
    .expect("resolves");
    assert_eq!(
        files_of(&set),
        json!({
            "extra": { "shape": "directory", "files": [
                { "name": "extra/onnx/encoder.onnx", "bytes": 3 },
                { "name": "extra/tokenizer.json", "bytes": 2 }
            ] },
            "model": { "shape": "file", "files": [{ "name": "model", "bytes": 7 }] }
        })
    );
}

#[test]
fn slots_the_manifest_does_not_declare_or_that_are_left_empty_are_refused_by_reason() {
    let dir = scratch("refusals");
    let model = dir.join("model.bin");
    write(&model, b"w");
    let unknown = resolve(
        &slots(),
        &[("model", &model), ("vocab", &model)],
        &ResourceRoot::Any,
    )
    .expect_err("unknown");
    assert_eq!(
        reason(&unknown),
        (
            codes::RESOURCE_INVALID,
            Some("vocab"),
            Some(slot_reasons::UNKNOWN_SLOT)
        )
    );
    let missing = resolve(&slots(), &[], &ResourceRoot::Any).expect_err("missing");
    assert_eq!(
        reason(&missing),
        (
            codes::RESOURCE_INVALID,
            Some("model"),
            Some(slot_reasons::MISSING)
        )
    );
    let shape = resolve(&slots(), &[("model", &dir)], &ResourceRoot::Any).expect_err("shape");
    assert_eq!(
        reason(&shape),
        (
            codes::RESOURCE_INVALID,
            Some("model"),
            Some(slot_reasons::SHAPE)
        )
    );
    let shape = resolve(
        &slots(),
        &[("model", &model), ("extra", &model)],
        &ResourceRoot::Any,
    )
    .expect_err("shape");
    assert_eq!(
        reason(&shape),
        (
            codes::RESOURCE_INVALID,
            Some("extra"),
            Some(slot_reasons::SHAPE)
        )
    );
    let absent = resolve(
        &slots(),
        &[("model", &dir.join("absent"))],
        &ResourceRoot::Any,
    )
    .expect_err("absent");
    assert_eq!(absent.code, codes::FETCH_FAILED);
}

#[test]
fn a_file_past_the_slots_cap_is_refused_at_load_and_one_in_a_directory_where_it_is_read() {
    let dir = scratch("cap");
    let model = dir.join("model.bin");
    write(&model, b"12345");
    let extra = dir.join("extra");
    write(&extra.join("big"), b"123456");
    write(&extra.join("small"), b"1");
    let capped = manifest(&json!([
        { "name": "model", "supplied": { "shape": "file", "max_bytes": 4 } },
        { "name": "extra", "supplied": { "shape": "directory", "max_bytes": 4 } }
    ]));
    let refused = resolve(
        &capped,
        &[("model", &model), ("extra", &extra)],
        &ResourceRoot::Any,
    )
    .expect_err("too large");
    assert_eq!(
        reason(&refused),
        (
            codes::RESOURCE_INVALID,
            Some("model"),
            Some(slot_reasons::TOO_LARGE)
        )
    );
    let small = dir.join("small.bin");
    write(&small, b"1234");
    let set = resolve(
        &capped,
        &[("model", &small), ("extra", &extra)],
        &ResourceRoot::Any,
    )
    .expect("the directory's files are checked where they are read");
    let mut reader = ResourceReader::new(set, CancelToken::default());
    assert_eq!(reader.bytes("extra/small"), Some(&b"1"[..]));
    assert_eq!(reader.bytes("extra/big"), None);
    let latched = reader.take_failure().expect("latched");
    assert_eq!(
        reason(&latched),
        (
            codes::RESOURCE_INVALID,
            Some("extra"),
            Some(slot_reasons::TOO_LARGE)
        )
    );
}

#[cfg(unix)]
#[test]
fn a_wasm_module_takes_no_supplied_file_past_2_gib() {
    let dir = scratch("wasm-cap");
    let model = dir.join("model.bin");
    std::fs::File::create(&model)
        .and_then(|file| file.set_len((2 << 30) + 1))
        .expect("sparse file");
    let refused = ResourceSet::resolve(
        &slots(),
        &supplied(&[("model", &model)]),
        &ResourceRoot::Any,
        true,
    )
    .expect_err("too large for wasm");
    assert_eq!(
        reason(&refused),
        (
            codes::RESOURCE_INVALID,
            Some("model"),
            Some(slot_reasons::TOO_LARGE)
        )
    );
    ResourceSet::resolve(
        &slots(),
        &supplied(&[("model", &model)]),
        &ResourceRoot::Any,
        false,
    )
    .expect("native takes it");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

#[test]
fn the_enumeration_never_trims_and_refuses_each_limit_by_reason() {
    let dir = scratch("limits");
    let model = dir.join("model.bin");
    write(&model, b"w");
    let many = dir.join("many");
    for index in 0..=alpha2_model::MAX_SUPPLIED_FILES {
        write(&many.join(format!("{index:04}")), b"");
    }
    let deep = (0..=MAX_SLOT_DEPTH).fold(dir.join("deep"), |path, level| {
        path.join(format!("d{level}"))
    });
    write(&deep.join("leaf"), b"");
    let long = dir.join("long");
    write(&long.join("n".repeat(255)).join("m".repeat(255)), b"");
    for (extra, expected) in [
        (&many, slot_reasons::TOO_MANY_FILES),
        (&dir.join("deep"), slot_reasons::TOO_DEEP),
        (&long, slot_reasons::NAME_TOO_LONG),
    ] {
        let refused = resolve(
            &slots(),
            &[("model", &model), ("extra", extra)],
            &ResourceRoot::Any,
        )
        .expect_err(expected);
        assert_eq!(
            reason(&refused),
            (codes::RESOURCE_INVALID, Some("extra"), Some(expected))
        );
    }
    let fits = (0..MAX_SLOT_DEPTH).fold(dir.join("fits"), |path, level| {
        path.join(format!("d{level}"))
    });
    write(&fits.join("leaf"), b"");
    resolve(
        &slots(),
        &[("model", &model), ("extra", &dir.join("fits"))],
        &ResourceRoot::Any,
    )
    .expect("eight levels fit");
}

#[cfg(unix)]
#[test]
fn links_are_followed_by_name_and_loops_broken_links_and_odd_names_are_refused() {
    use std::os::unix::fs::symlink;
    let dir = scratch("links");
    let model = dir.join("model.bin");
    write(&model, b"w");
    let hub = dir.join("hub");
    write(&hub.join("blobs/aaaa"), b"encoder");
    write(&hub.join("blobs/bbbb"), b"data");
    let snapshot = hub.join("snapshots/rev");
    std::fs::create_dir_all(snapshot.join("onnx")).expect("dir");
    symlink("../../../blobs/aaaa", snapshot.join("onnx/model.onnx")).expect("link");
    symlink("../../../blobs/bbbb", snapshot.join("onnx/model.onnx_data")).expect("link");
    symlink("onnx", snapshot.join("alias")).expect("dir link");
    let set = resolve(
        &slots(),
        &[("model", &model), ("extra", &snapshot)],
        &ResourceRoot::Any,
    )
    .expect("a snapshot of links resolves");
    assert_eq!(
        files_of(&set)["extra"]["files"],
        json!([
            { "name": "extra/alias/model.onnx", "bytes": 7 },
            { "name": "extra/alias/model.onnx_data", "bytes": 4 },
            { "name": "extra/onnx/model.onnx", "bytes": 7 },
            { "name": "extra/onnx/model.onnx_data", "bytes": 4 }
        ])
    );
    let mut reader = ResourceReader::new(set, CancelToken::default());
    let path = reader
        .path("extra/onnx")
        .expect("a directory's path")
        .to_owned();
    let root = std::fs::canonicalize(&snapshot).expect("canonical");
    assert_eq!(Path::new(&path), root.join("onnx"));
    assert_eq!(
        reader
            .supplied()
            .iter()
            .map(|file| (file.name.as_str(), file.bytes))
            .collect::<Vec<_>>(),
        [
            ("extra/onnx/model.onnx", 7),
            ("extra/onnx/model.onnx_data", 4)
        ]
    );
    assert_eq!(
        reader.supplied()[0].sha256,
        sha256_hex(b"encoder"),
        "hashed by content, recorded by name"
    );

    let looped = dir.join("looped");
    write(&looped.join("a/file"), b"");
    symlink("..", looped.join("a/up")).expect("loop");
    let broken = dir.join("broken");
    std::fs::create_dir_all(&broken).expect("dir");
    symlink("nowhere", broken.join("gone")).expect("broken link");
    let mut cases = vec![
        (looped, slot_reasons::TOO_DEEP),
        (broken, slot_reasons::MISSING),
    ];
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let odd = dir.join("odd");
        std::fs::create_dir_all(&odd).expect("dir");
        std::fs::write(odd.join(std::ffi::OsStr::from_bytes(b"bad\xffname")), b"")
            .expect("a name that is not UTF-8");
        cases.push((odd, slot_reasons::NOT_UTF8));
    }
    for (extra, expected) in cases {
        let refused = resolve(
            &slots(),
            &[("model", &model), ("extra", &extra)],
            &ResourceRoot::Any,
        )
        .expect_err(expected);
        assert_eq!(
            reason(&refused),
            (codes::RESOURCE_INVALID, Some("extra"), Some(expected))
        );
    }
}

#[cfg(unix)]
#[test]
fn a_resource_root_takes_only_files_whose_links_resolve_under_it() {
    use std::os::unix::fs::symlink;
    let dir = scratch("root");
    let model = dir.join("allowed/model.bin");
    write(&model, b"w");
    let outside = dir.join("outside.bin");
    write(&outside, b"o");
    let snapshot = dir.join("allowed/snapshot");
    std::fs::create_dir_all(&snapshot).expect("dir");
    symlink(&outside, snapshot.join("weights")).expect("link out");
    let root = ResourceRoot::Dir(dir.join("allowed"));
    resolve(&slots(), &[("model", &model)], &root).expect("under the root");
    for entries in [
        vec![("model", outside.as_path())],
        vec![("model", model.as_path()), ("extra", snapshot.as_path())],
    ] {
        let refused = resolve(&slots(), &entries, &root).expect_err("outside");
        assert_eq!(refused.code, codes::REFUSED, "{refused:?}");
    }
    let none = resolve(&slots(), &[("model", &model)], &ResourceRoot::None).expect_err("none");
    assert_eq!(none.code, codes::REFUSED);
    resolve(&slots(), &[("model", &outside)], &ResourceRoot::Any).expect("any");
}

#[test]
fn a_supplied_file_that_changes_between_reads_fails_the_read() {
    let dir = scratch("changed");
    let model = dir.join("model.bin");
    write(&model, b"before");
    let set = resolve(&slots(), &[("model", &model)], &ResourceRoot::Any).expect("resolves");
    let mut reader = ResourceReader::new(set, CancelToken::default());
    let mut target = [0; 6];
    assert!(reader.read_into("model", &mut target));
    assert_eq!(&target, b"before");
    write(&model, b"after!");
    assert!(!reader.read_into("model", &mut target));
    assert_eq!(
        reader.take_failure().map(|error| error.code),
        Some(codes::FETCH_FAILED)
    );
    write(&model, b"longer now");
    assert!(reader.path("model").is_none());
    let failure = reader.take_failure().expect("latched");
    assert_eq!(failure.code, codes::FETCH_FAILED);
    assert!(failure.message.contains("changed"), "{}", failure.message);
}

#[test]
fn a_pinned_file_replaced_after_the_load_checked_it_fails_where_it_is_read() {
    let dir = scratch("pinned");
    let vocab = dir.join("vocab.json");
    write(&vocab, b"{}");
    let mut set = ResourceSet::default();
    set.add_pinned(
        "vocab",
        PinnedFile {
            path: vocab.clone(),
            sha256: sha256_hex(b"{}"),
            bytes: 2,
            local: true,
        },
    );
    let mut reader = ResourceReader::new(set, CancelToken::default());
    assert_eq!(reader.bytes("vocab"), Some(&b"{}"[..]));
    let mut fresh = ResourceSet::default();
    fresh.add_pinned(
        "vocab",
        PinnedFile {
            path: vocab.clone(),
            sha256: sha256_hex(b"{}"),
            bytes: 2,
            local: true,
        },
    );
    write(&vocab, b"[]");
    let mut reader = ResourceReader::new(fresh, CancelToken::default());
    assert_eq!(reader.bytes("vocab"), None);
    assert_eq!(
        reader.take_failure().map(|error| error.code),
        Some(codes::HASH_MISMATCH)
    );
    assert!(reader.path("vocab").is_none(), "a pinned file has no path");
    assert!(reader.take_failure().is_none());
    assert_eq!(reader.supplied(), Vec::new());
}

#[test]
fn a_read_stops_at_the_cancel_and_at_the_calls_deadline() {
    let dir = scratch("stops");
    let model = dir.join("model.bin");
    write(&model, &vec![7; 3 << 20]);
    let set = resolve(&slots(), &[("model", &model)], &ResourceRoot::Any).expect("resolves");
    let cancel = CancelToken::default();
    let mut reader = ResourceReader::new(set.clone(), cancel.clone());
    cancel.cancel();
    assert_eq!(reader.bytes("model"), None);
    assert_eq!(
        reader.take_failure().map(|error| error.code),
        Some(CANCELLED)
    );
    let mut reader = ResourceReader::new(set, CancelToken::default());
    reader.start_call(Some((Instant::now(), Duration::ZERO)));
    assert_eq!(reader.size("model"), Some(3 << 20));
    let mut target = vec![0; 3 << 20];
    assert!(!reader.read_into("model", &mut target));
    assert_eq!(
        reader.take_failure().map(|error| error.code),
        Some(codes::LIMIT)
    );
    reader.start_call(None);
    assert!(reader.read_into("model", &mut target));
    assert!(target.iter().all(|byte| *byte == 7));
}

#[test]
fn paths_name_supplied_files_and_directories_only() {
    let dir = scratch("paths");
    let model = dir.join("model.bin");
    write(&model, b"w");
    let extra = dir.join("extra");
    write(&extra.join("sub/a"), b"a");
    write(&extra.join("b"), b"b");
    let set = resolve(
        &slots(),
        &[("model", &model), ("extra", &extra)],
        &ResourceRoot::Any,
    )
    .expect("resolves");
    let mut reader = ResourceReader::new(set, CancelToken::default());
    let canonical = |path: &Path| std::fs::canonicalize(path).expect("canonical");
    assert_eq!(
        reader.path("model").map(PathBuf::from),
        Some(canonical(&model))
    );
    assert_eq!(
        reader.path("extra").map(PathBuf::from),
        Some(canonical(&extra))
    );
    assert_eq!(
        reader.path("extra/sub").map(PathBuf::from),
        Some(canonical(&extra).join("sub"))
    );
    for absent in [
        "extra/none",
        "extra/sub/..",
        "extra/",
        "vocab",
        "",
        "extra//sub",
    ] {
        assert_eq!(reader.path(absent), None, "{absent}");
    }
    assert!(reader.take_failure().is_none());
    assert_eq!(
        reader
            .supplied()
            .into_iter()
            .map(|file| file.name)
            .collect::<Vec<_>>(),
        ["extra/b", "extra/sub/a", "model"]
    );
}
