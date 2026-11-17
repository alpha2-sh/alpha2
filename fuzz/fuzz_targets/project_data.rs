#![no_main]

use alpha2_eval::{ActiveIndex, evaluate};
use alpha2_time::MediaTime;
use libfuzzer_sys::fuzz_target;

// Any valid project with data sets and bindings evaluates at its data's times without panicking,
// and its data survives a JSON round trip unchanged.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(project) = alpha2_model::Project::from_json(text) else {
        return;
    };
    let reloaded = serde_json::to_string(&project).expect("a project serializes");
    let reloaded = alpha2_model::Project::from_json(&reloaded).expect("a valid project reloads");
    assert_eq!(reloaded.data, project.data);
    let Ok(index) = ActiveIndex::build(&project) else {
        return;
    };
    let mut times = vec![0];
    for set in &project.data {
        if let Some(samples) = set.values.samples() {
            times.extend(samples.times.iter().take(4).map(|time| time.ticks()));
        }
    }
    for time in times {
        for nudge in [-1, 0, 1] {
            let _ = evaluate(&project, &index, MediaTime(time.saturating_add(nudge)));
        }
    }
});
