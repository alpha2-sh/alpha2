#![no_main]

//! Lottie JSON files and dotLottie packages through the Lottie provider: probe, open the animation
//! the probe describes, render (see `extension_common`).

#[path = "extension_common.rs"]
mod common;

use alpha2_extensions::LottieProvider;
use alpha2_model::ProviderSource;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|bytes: &[u8]| {
    common::exercise(&LottieProvider, bytes, |report| ProviderSource::Lottie {
        animation: report.item.clone(),
    });
});
