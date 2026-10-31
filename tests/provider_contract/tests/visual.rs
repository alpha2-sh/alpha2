//! The extension providers keep the visual provider contract
//! (`alpha2_provider_contract::visual`) over the fixture's cases, each
//! reaching the conclusion the fixture lists (the browser module is checked
//! against the same in `scripts/test-wasm-browser.mjs`), and over every file
//! of the extension corpus.
#![cfg(feature = "visual")]

use alpha2_extensions::{
    AnimatedImageProvider, ExrProvider, LottieProvider, SvgProvider, VisualProvider,
};
use alpha2_provider_contract::visual::{Conclusion, VisualContract, conclude};

/// The providers, in the order a host registers them.
fn providers() -> [&'static dyn VisualProvider; 4] {
    [
        &SvgProvider,
        &AnimatedImageProvider,
        &LottieProvider,
        &ExrProvider,
    ]
}

/// A conclusion as the fixture writes it (to write a new case's).
fn fixture(conclusion: &Conclusion) -> serde_json::Value {
    let mut value = serde_json::json!({ "probe": conclusion.probe });
    if let Some(provider) = &conclusion.provider {
        value["provider"] = provider.clone().into();
    }
    if let Some(open) = &conclusion.open {
        value["open"] = open.clone().into();
    }
    if let Some(size) = conclusion.size {
        value["size"] = serde_json::json!(size);
    }
    if let Some(frames) = conclusion.frames {
        value["frames"] = frames.into();
    }
    if !conclusion.renders.is_empty() {
        value["renders"] = conclusion
            .renders
            .iter()
            .map(|(frame, fit, answer)| match answer {
                Ok(size) => serde_json::json!({ "frame": frame, "fit": fit, "size": size }),
                Err(refused) => {
                    serde_json::json!({ "frame": frame, "fit": fit, "refused": refused })
                }
            })
            .collect();
    }
    value
}

#[test]
fn each_case_reaches_the_conclusion_the_fixture_lists() {
    let contract = VisualContract::load();
    assert!(contract.cases.len() >= 20, "the fixture's cases");
    let mut wrong = Vec::new();
    for case in &contract.cases {
        // On its own thread, with the stack a browser module has.
        let reached = std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(if cfg!(debug_assertions) {
                    16 << 20
                } else {
                    1 << 20
                })
                .spawn_scoped(scope, || {
                    conclude(
                        &providers(),
                        &case.bytes(),
                        &case.render_requests(),
                        &case.name,
                    )
                })
                .expect("thread")
                .join()
                .unwrap_or_else(|_| panic!("{}: broke the contract", case.name))
        });
        if reached != case.conclusion() {
            wrong.push(format!("{}: {}", case.name, fixture(&reached)));
        }
    }
    assert!(
        wrong.is_empty(),
        "concluded otherwise:\n{}",
        wrong.join("\n")
    );
}

#[test]
fn every_extension_corpus_file_keeps_the_contract() {
    let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../corpus/extensions");
    let mut read = 0;
    for entry in std::fs::read_dir(&corpus).expect("the extension corpus") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        let extension = path.extension().and_then(|extension| extension.to_str());
        if path.is_dir() || matches!(extension, Some("rgba" | "py")) || name == "MANIFEST.json" {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read");
        let _ = conclude(
            &providers(),
            &bytes,
            &[(0, [0, 0]), (0, [256, 256]), (usize::MAX, [97, 61])],
            name,
        );
        read += 1;
    }
    assert!(read >= 20, "{read} files");
}
