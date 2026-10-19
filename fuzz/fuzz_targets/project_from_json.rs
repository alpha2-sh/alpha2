#![no_main]

use libfuzzer_sys::fuzz_target;

// Any UTF-8 text either parses as a project or is refused; parsing never panics.
fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = alpha2_model::Project::from_json(text);
    }
});
