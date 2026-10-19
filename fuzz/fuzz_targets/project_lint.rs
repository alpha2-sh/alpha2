#![no_main]

use libfuzzer_sys::fuzz_target;

// Any project that loads lints, as `lintProject` does in the browser: the diagnostics never panic,
// are the same each time, and serialize as the JSON the binding returns.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(project) = alpha2_model::Project::from_json(text) else {
        return;
    };
    let issues = project.lint();
    assert_eq!(issues, project.lint());
    serde_json::to_string(&issues).expect("lint issues serialize");
});
