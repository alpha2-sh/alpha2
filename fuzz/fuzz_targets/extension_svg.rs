#![no_main]

//! SVG documents through the SVG provider: probe, open, render (see `extension_common`).

#[path = "extension_common.rs"]
mod common;

use alpha2_extensions::SvgProvider;
use alpha2_model::ProviderSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    common::exercise(&SvgProvider, bytes, |_| ProviderSource::Svg);
});
