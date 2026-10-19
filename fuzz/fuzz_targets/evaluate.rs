#![no_main]

use alpha2_engine::{AudioRenderer, EngineProject, MAX_AUDIO_BLOCK};
use alpha2_time::{MediaTime, TimeRange};
use libfuzzer_sys::fuzz_target;

// Any valid project, evaluated at any time, answers or refuses: it never panics. Seed with the
// project JSON under `tests/`.
fuzz_target!(|input: (i64, i64, &str)| {
    let (time, span, json) = input;
    let Ok(project) = alpha2_model::Project::from_json(json) else {
        return;
    };
    let Ok(snapshot) = EngineProject::new(project) else {
        return;
    };
    let time = MediaTime::from_ticks(time);
    let _ = snapshot.evaluate(time);
    let _ = snapshot.frame_needs(time);
    if let Some(end) = time
        .ticks()
        .checked_add(span.unsigned_abs().min(1 << 40) as i64)
        && let Ok(range) = TimeRange::new(time, MediaTime::from_ticks(end))
    {
        let _ = snapshot.evaluate_audio(range);
    }
    if let Ok(renderer) = AudioRenderer::new(&snapshot, 2, MAX_AUDIO_BLOCK) {
        let frames = usize::try_from(span.unsigned_abs() % MAX_AUDIO_BLOCK as u64).unwrap_or(0) + 1;
        let _ = renderer.audio_needs(&snapshot, time.ticks(), frames);
    }
});
