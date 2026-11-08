#![no_main]

use alpha2_engine::extract::{ArtifactKind, PluginManifest};
use libfuzzer_sys::fuzz_target;

// Any text parses as a plugin manifest or is refused; an accepted manifest picks artifacts without
// panicking and survives a JSON round trip.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(manifest) = PluginManifest::from_json(text) else {
        return;
    };
    for target in ["wasm32", "x86_64-unknown-linux-gnu", "aarch64-apple-darwin"] {
        for kind in [ArtifactKind::Wasm, ArtifactKind::Cdylib] {
            let _ = manifest.artifact(target, kind);
        }
    }
    let again = serde_json::to_string(&manifest).expect("a manifest serializes");
    let again = PluginManifest::from_json(&again).expect("an accepted manifest reloads");
    assert_eq!(again, manifest);
});
