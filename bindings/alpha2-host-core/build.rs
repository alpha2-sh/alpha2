//! Records the target triple, which picks a plugin's native artifact.

fn main() {
    let target = std::env::var("TARGET").unwrap_or_default();
    println!("cargo:rustc-env=ALPHA2_HOST_CORE_TARGET={target}");
}
