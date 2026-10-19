#![no_main]

//! GIF, animated WebP, and APNG files through the animated image provider: probe, open, render
//! (see `extension_common`).

#[path = "extension_common.rs"]
mod common;

use alpha2_extensions::AnimatedImageProvider;
use alpha2_model::ProviderSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    common::exercise(&AnimatedImageProvider, bytes, |_| {
        ProviderSource::AnimatedImage
    });
});
