//! Links clang's runtime for the Metal build on Apple silicon.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=CC");
    let target = |key: &str| std::env::var(key).unwrap_or_default();
    if target("CARGO_CFG_TARGET_OS") != "macos" || target("CARGO_CFG_TARGET_ARCH") != "aarch64" {
        return;
    }
    // ggml's Metal code calls clang's `__isPlatformVersionAtLeast`; under fat LTO an executable
    // drops the weak copy in Rust's std, so clang's runtime archive provides it.
    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_owned());
    let output = Command::new(&compiler)
        .arg("-print-resource-dir")
        .output()
        .unwrap_or_else(|error| panic!("{compiler} -print-resource-dir: {error}"));
    assert!(
        output.status.success(),
        "{compiler} -print-resource-dir: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let resources = String::from_utf8(output.stdout).expect("a UTF-8 path");
    println!(
        "cargo:rustc-link-search=native={}/lib/darwin",
        resources.trim()
    );
    println!("cargo:rustc-link-lib=static=clang_rt.osx");
}
