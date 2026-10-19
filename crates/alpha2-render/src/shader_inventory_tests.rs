#[test]
fn every_built_module_is_listed_for_the_browser_compile() {
    // Each complete module the shaders crate builds, the browser binding's canvas
    // presenter among them, reaches test:wgsl-browser through this list.
    let listed = super::builtin_wgsl_modules()
        .into_iter()
        .map(|(label, _)| label)
        .collect::<Vec<_>>();
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../alpha2-shaders/shaders/build");
    for entry in std::fs::read_dir(&directory).expect("shader build directory") {
        let path = entry.expect("shader entry").path();
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("name");
        // `fullscreen` is a library the others import, not a module.
        if name != "fullscreen" {
            assert!(
                listed.iter().any(|label| label == name),
                "{name} is not listed"
            );
        }
    }
}
