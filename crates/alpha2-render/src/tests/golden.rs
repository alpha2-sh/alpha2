//! The shared PNG goldens (`alpha2-golden-support`), taking a render size, for runs on the suite's
//! shared device.

use alpha2_golden_support::{GoldenRun, Scope, Variant};

use crate::RenderSize;

/// The run this suite renders on: the shared device, with the intermediate `gpu()` gives.
fn run() -> GoldenRun {
    let gpu = super::gpu();
    let capabilities = gpu.capabilities();
    GoldenRun {
        renderer: "gpu",
        adapter: capabilities.adapter_name.clone(),
        backend: capabilities.backend.clone(),
        driver: format!("{} {}", capabilities.driver, capabilities.driver_info)
            .trim()
            .to_owned(),
        shader_compiler: capabilities.shader_compiler.clone(),
        intermediate: format!("{:?}", capabilities.intermediate_format),
        software: capabilities.software_rasterizer,
        profile: GoldenRun::profile(),
    }
}

/// A variant for runs on `backend`.
pub(crate) const fn on_backend(name: &'static str, backend: &'static str) -> Variant {
    Variant {
        name,
        scope: Scope {
            backend: Some(backend),
            adapter_contains: None,
            intermediate: None,
        },
    }
}

/// A variant for runs on `backend` whose adapter is named like `adapter`.
pub(crate) const fn on_adapter(
    name: &'static str,
    backend: &'static str,
    adapter: &'static str,
) -> Variant {
    Variant {
        name,
        scope: Scope {
            backend: Some(backend),
            adapter_contains: Some(adapter),
            intermediate: None,
        },
    }
}

pub(crate) fn assert_rgba8_golden(
    name: &'static str,
    size: RenderSize,
    actual: &[u8],
    tolerance: u8,
) {
    alpha2_golden_support::assert_rgba8_golden(
        &run(),
        name,
        (size.width, size.height),
        actual,
        tolerance,
    );
}

pub(crate) fn assert_rgba8_golden_tiles(
    variants: &[Variant],
    size: RenderSize,
    tiles: &[&str],
    actual: &[u8],
    tolerance: u8,
) {
    alpha2_golden_support::assert_rgba8_golden_tiles(
        &run(),
        variants,
        (size.width, size.height),
        tiles,
        actual,
        tolerance,
    );
}
