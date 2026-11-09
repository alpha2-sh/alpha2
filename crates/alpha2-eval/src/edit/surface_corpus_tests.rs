//! `ALPHA2_UPDATE_GOLDENS=1` rewrites the expected answers.

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::apply_json;

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

fn path() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/cases/surface-conformance.json")
}

fn answer(projects: &Value, case: &Value) -> Value {
    let project = &projects[case["project"].as_str().expect("project")];
    match apply_json(&project.to_string(), &case["request"].to_string()) {
        Ok(edited) => json!({
            "project_sha256": hex(&Sha256::digest(edited.project.as_bytes())),
            "report": serde_json::from_str::<Value>(&edited.report).expect("report"),
        }),
        Err(failure) => json!({ "code": failure.code, "refusal": failure.refusal }),
    }
}

#[test]
fn every_edit_case_answers_as_pinned() {
    let mut table: Value =
        serde_json::from_slice(&std::fs::read(path()).expect("the table")).expect("json");
    let projects = table["projects"].clone();
    let update =
        std::env::var_os("ALPHA2_UPDATE_GOLDENS").as_deref() == Some(std::ffi::OsStr::new("1"));
    let mut stale = Vec::new();
    for case in table["edit"].as_array_mut().expect("edit cases") {
        let found = answer(&projects, case);
        if case["expected"] != found {
            stale.push(case["name"].as_str().unwrap_or_default().to_owned());
            case["expected"] = found;
        }
    }
    if update {
        let mut text = serde_json::to_string_pretty(&table).expect("json");
        text.push('\n');
        std::fs::write(path(), text).expect("write the table");
        return;
    }
    assert!(
        stale.is_empty(),
        "stale edit cases {stale:?}; rerun with ALPHA2_UPDATE_GOLDENS=1"
    );
}

#[test]
fn the_edit_cases_cover_every_refusal() {
    let table: Value =
        serde_json::from_slice(&std::fs::read(path()).expect("the table")).expect("json");
    let cases = table["edit"].as_array().expect("edit cases");
    let refusals = cases
        .iter()
        .filter_map(|case| case["expected"]["refusal"].as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(refusals.len(), 9, "{refusals:?}");
    for case in cases {
        let name = case["name"].as_str().expect("name");
        assert_eq!(
            name.starts_with("refuse-"),
            case["expected"]["refusal"].is_string(),
            "{name}"
        );
    }
}
