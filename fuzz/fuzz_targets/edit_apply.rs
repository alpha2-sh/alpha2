#![no_main]

use alpha2_eval::edit::{EditError, EditRequest, apply};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

// Any edit request, against any valid project (`{"project": ..., "request": ...}`), errors or
// gives a project that validates: the operations never panic and never produce an invalid
// document, and the same input gives the same output.
fuzz_target!(|data: &[u8]| {
    let Ok(input) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    let Ok(project) = alpha2_model::Project::from_json(&input["project"].to_string()) else {
        return;
    };
    let Ok(request) = serde_json::from_value::<EditRequest>(input["request"].clone()) else {
        return;
    };
    match apply(&project, &request) {
        Ok((edited, report)) => {
            let text = serde_json::to_string(&edited).expect("an edited project serializes");
            let reloaded =
                alpha2_model::Project::from_json(&text).expect("an edited project reloads");
            assert_eq!(reloaded, edited);
            assert_eq!(apply(&project, &request).ok(), Some((edited, report)));
        }
        Err(EditError::InvalidResult(error)) => {
            panic!("an edit produced an invalid project: {error}")
        }
        Err(_) => {}
    }
});
