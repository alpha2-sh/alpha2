#![no_main]

//! OpenEXR images through the OpenEXR provider: probe, open, render (see `extension_common`).

#[path = "extension_common.rs"]
mod common;

use alpha2_extensions::ExrProvider;
use alpha2_model::ProviderSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    common::exercise(&ExrProvider, bytes, |_| ProviderSource::Exr {
        primaries: None,
    });
});
