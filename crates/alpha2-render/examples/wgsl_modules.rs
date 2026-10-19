//! Prints every WGSL module the renderer can create as a JSON array of
//! `{ "label", "source" }`, for `scripts/test-wgsl-browser.mjs` to compile in
//! a browser.

fn main() {
    let modules = alpha2_render::builtin_wgsl_modules()
        .into_iter()
        .map(|(label, source)| serde_json::json!({ "label": label, "source": source }))
        .collect::<Vec<_>>();
    println!(
        "{}",
        serde_json::to_string(&modules).expect("modules serialize")
    );
}
