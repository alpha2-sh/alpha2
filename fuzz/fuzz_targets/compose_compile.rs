#![no_main]

use libfuzzer_sys::fuzz_target;

// Facts about every file the compose cases name, so seeds reach the compiler.
const FACTS: &str = r#"{"media": {
  "slides/01.jpg": {"width": 2400, "height": 1600}, "slides/02.jpg": {"width": 2400, "height": 1600},
  "slides/03.jpg": {"width": 2400, "height": 1600}, "slides/04.jpg": {"width": 2400, "height": 1600},
  "slides/05.jpg": {"width": 2400, "height": 1600}, "slides/06.jpg": {"width": 2400, "height": 1600},
  "slides/07.jpg": {"width": 2400, "height": 1600}, "slides/08.jpg": {"width": 2400, "height": 1600},
  "slides/09.jpg": {"width": 2400, "height": 1600}, "slides/10.jpg": {"width": 2400, "height": 1600},
  "slides/11.jpg": {"width": 2400, "height": 1600}, "slides/12.jpg": {"width": 2400, "height": 1600},
  "music.m4a": {"duration_ticks": 8400000, "has_audio": true},
  "talk.mp4": {"width": 1080, "height": 1920, "frame_rate": {"num": 30, "den": 1}, "duration_ticks": 7200000, "has_audio": true},
  "logo.png": {"width": 512, "height": 512},
  "wide.mp4": {"width": 1920, "height": 1080, "frame_rate": {"num": 30, "den": 1}, "duration_ticks": 1200000, "has_audio": true}
}}"#;

// A compose spec compiles or is refused with where and why, never panics, and a compiled project
// is one the engine loads. Text after a NUL byte replaces the facts.
fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let (spec, facts) = text.split_once('\0').unwrap_or((text, FACTS));
    let Ok(compiled) = alpha2_compose::compile_json(spec, facts, &[]) else {
        return;
    };
    let compiled: serde_json::Value = serde_json::from_str(&compiled).expect("compiled JSON");
    if let Err(error) = alpha2_model::Project::from_json(&compiled["project"].to_string()) {
        panic!("a compiled project does not load: {error}");
    }
});
