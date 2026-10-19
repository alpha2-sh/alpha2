//! Builds the D3D12 copy pool (`c/d3d12.c`) and encode copier
//! (`c/d3d12_encode.c`) against stand-in COM objects (`d3d12_pool.c`,
//! `d3d12_mock/`) with the C compiler the build uses, and runs it. Their
//! bookkeeping (which idle target a full pool frees, what a trim frees and
//! reports, what a copy still out keeps; the copier's slot rotation, a reset
//! only after a slot's fence, a teardown with a slot still pending, and what
//! each failing step leaves) needs no device, so it is checked on Linux on
//! every run; the copies themselves are checked on Windows by
//! `src/d3d12_tests.rs`.
#![cfg(target_os = "linux")]

use std::{env, path::Path, process::Command};

#[test]
fn the_d3d12_copy_pool_keeps_the_sizes_shown_now_and_trims_its_idle_targets() {
    let package = Path::new(env!("CARGO_MANIFEST_DIR"));
    let harness = Path::new(env!("CARGO_TARGET_TMPDIR")).join("d3d12_pool");
    let compiler = env::var_os("CC").unwrap_or_else(|| "cc".into());
    let built = Command::new(&compiler)
        .args(["-std=c11", "-Wall", "-Wextra", "-Werror", "-I"])
        .arg(package.join("tests/d3d12_mock"))
        .arg("-I")
        .arg(package.join("include"))
        .arg(package.join("tests/d3d12_pool.c"))
        .arg("-o")
        .arg(&harness)
        .output()
        .unwrap_or_else(|error| panic!("{} does not run: {error}", compiler.display()));
    assert!(
        built.status.success(),
        "the harness does not build:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let ran = Command::new(&harness).output().expect("the harness runs");
    let stdout = String::from_utf8_lossy(&ran.stdout);
    println!("{stdout}");
    assert!(
        ran.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&ran.stderr)
    );
}
